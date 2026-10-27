//! 持久化：会话元数据（`session.sqlite`）、会话 transcript（每会话一个
//! `transcript.sqlite`）与工作流（`workflow.sqlite`）。
//!
//! 各连接由 `Mutex` 保护；SQLite 查询同步执行（本地文件，量级小）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use amux_common::api::{
    parse_automation_run_state, AutomationInput, AutomationResultLabel, AutomationRun,
    AutomationSessionOptions, AutomationTrigger, GroupMessage, Session,
};
use amux_common::domain::{Activity, ContentBlock, HistoryItem, SessionState};
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
    groups: Mutex<Connection>,
    automations: Mutex<Connection>,
    transcripts: Mutex<TranscriptCache>,
    group_transcripts: Mutex<TranscriptCache>,
}

/// 新建工作流元数据所需的输入。
pub struct WorkflowInsert<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub state: SessionState,
    pub plan: &'a str,
    pub project: Option<&'a str>,
    pub created_at: u64,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self, String> {
        let home = home.to_path_buf();
        std::fs::create_dir_all(&home).map_err(|e| format!("创建 {} 失败: {e}", home.display()))?;
        let sessions = open_db(&home.join("session.sqlite"), SESSION_SCHEMA)?;
        let workflows = open_db(&home.join("workflow.sqlite"), WORKFLOW_SCHEMA)?;
        let groups = open_db(&home.join("group.sqlite"), GROUP_SCHEMA)?;
        let automations = open_db(&home.join("automation.sqlite"), AUTOMATION_SCHEMA)?;
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
        // Server 重启后残留的「工作中」不再有 agent 侧 turn 支撑，统一回到空闲
        groups
            .execute("UPDATE groups SET state = 'idle' WHERE state = 'busy'", [])
            .map_err(|e| format!("重置群聊会话状态失败: {e}"))?;
        Ok(Self {
            home,
            sessions: Mutex::new(sessions),
            workflows: Mutex::new(workflows),
            groups: Mutex::new(groups),
            automations: Mutex::new(automations),
            transcripts: Mutex::new(TranscriptCache::default()),
            group_transcripts: Mutex::new(TranscriptCache::default()),
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
                "INSERT INTO sessions (id, workflow_id, group_id, state, title, project, pinned, unread, workspace, worktree_dir, machine, agent, model, agent_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL, ?14, ?15)",
                params![
                    session.id,
                    session.workflow_id,
                    session.group_id,
                    session.state.as_str(),
                    session.title,
                    session.project.as_deref(),
                    session.pinned,
                    session.unread,
                    session.workspace,
                    session.worktree_dir,
                    session.machine,
                    session.agent,
                    session.model.as_deref(),
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
                "SELECT id, workflow_id, group_id, state, title, project, pinned, unread, workspace, worktree_dir, machine, agent, model, agent_session_id, created_at, updated_at
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
            "SELECT id, workflow_id, group_id, state, title, project, pinned, unread, workspace, worktree_dir, machine, agent, model, agent_session_id, created_at, updated_at
             FROM sessions ORDER BY pinned DESC, created_at DESC, id DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], row_to_session);
        collect(rows)
    }

    /// 会话列表：分页返回非关联、非置顶普通会话，并始终附带该项目全部置顶会话。
    ///
    /// `has_more` 只表示非置顶分页是否还有更早一页；置顶会话不占用分页窗口。
    pub fn sessions_page(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> (Vec<Session>, bool) {
        let sessions: Vec<Session> = self
            .sessions_all()
            .into_iter()
            .filter(|session| session.workflow_id.is_none() && session.group_id.is_none())
            .filter(|session| match project {
                None => true,
                Some("") => session.project.is_none(),
                Some(project) => session.project.as_deref() == Some(project),
            })
            .collect();
        let mut pinned: Vec<Session> = sessions
            .iter()
            .filter(|session| session.pinned)
            .cloned()
            .collect();
        let mut unpinned: Vec<Session> = sessions
            .into_iter()
            .filter(|session| !session.pinned)
            .collect();
        pinned.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        unpinned.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        let has_more = unpinned.len() > offset + limit;
        let mut page = pinned;
        page.extend(unpinned.into_iter().skip(offset).take(limit));
        (page, has_more)
    }

    pub fn sessions_of_agent(&self, machine: &str, agent: &str) -> Vec<Session> {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, workflow_id, group_id, state, title, project, pinned, unread, workspace, worktree_dir, machine, agent, model, agent_session_id, created_at, updated_at
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
        let workflow_id = self
            .sessions
            .lock()
            .query_row(
                "SELECT workflow_id FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()?;
        workflow_id
    }

    /// 成员会话所属群聊会话 id。
    pub fn group_of_session(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .query_row(
                "SELECT group_id FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()?
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

    pub fn set_title_if_empty(&self, id: &str, title: &str) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3
             WHERE id = ?1 AND (title IS NULL OR title = '')",
            params![id, title, now_ms() as i64],
        );
    }

    /// 会话活跃：更新最近活跃时间（会话详情展示用）。
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

    /// 原子认领 Agent 会话；并发创建时只有一个调用能成功。
    pub fn claim_agent_session_id(&self, id: &str, agent_session_id: &str) -> bool {
        self.sessions
            .lock()
            .execute(
                "UPDATE sessions SET agent_session_id = ?2
                 WHERE id = ?1 AND agent_session_id IS NULL",
                params![id, agent_session_id],
            )
            .is_ok_and(|changed| changed == 1)
    }

    pub fn delete_session(&self, id: &str) {
        let conn = self.sessions.lock();
        let _ = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id]);
        drop(conn);
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
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET workflow_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, workflow_id, now_ms() as i64],
        );
    }

    /// 标记群聊成员会话（供 `GET /sessions` 排除）。
    pub fn link_group_session(&self, group_id: &str, session_id: &str) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET group_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, group_id, now_ms() as i64],
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

    /// 读取磁盘中的消息内容，应用流式更新后立即写回。
    pub fn update_message(
        &self,
        session_id: &str,
        message_id: &str,
        role: &str,
        created_at: u64,
        update: impl FnOnce(Option<Vec<ContentBlock>>) -> Vec<ContentBlock>,
    ) {
        let transcript = match self.transcript(session_id, true) {
            Ok(Some(transcript)) => transcript,
            Err(error) => {
                log::warn!("打开会话 transcript 失败（{session_id}）: {error}");
                return;
            }
            Ok(None) => return,
        };
        let conn = transcript.lock();
        let current = conn
            .query_row(
                "SELECT content FROM messages WHERE message_id = ?1",
                params![message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|content| serde_json::from_str(&content).ok());
        let content = serde_json::to_string(&update(current)).unwrap_or_default();
        let _ = conn.execute(
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

    /// 读取磁盘中的活动内容，应用流式更新后立即写回。
    pub fn update_activity(
        &self,
        session_id: &str,
        activity_id: &str,
        kind: &str,
        created_at: u64,
        update: impl FnOnce(Option<Activity>) -> Activity,
    ) {
        let transcript = match self.transcript(session_id, true) {
            Ok(Some(transcript)) => transcript,
            Err(error) => {
                log::warn!("打开会话 transcript 失败（{session_id}）: {error}");
                return;
            }
            Ok(None) => return,
        };
        let conn = transcript.lock();
        let current = conn
            .query_row(
                "SELECT content FROM activities WHERE activity_id = ?1",
                params![activity_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
            .and_then(|content| serde_json::from_str(&content).ok());
        let content = serde_json::to_string(&update(current)).unwrap_or_default();
        let _ = conn.execute(
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

    /// 按活动 ID 读取单条活动内容（实时活动用）。
    pub fn activity_by_id(&self, session_id: &str, activity_id: &str) -> Option<Activity> {
        let transcript = self.transcript(session_id, false).ok()??;
        let content = transcript
            .lock()
            .query_row(
                "SELECT content FROM activities WHERE activity_id = ?1",
                params![activity_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten();
        content.and_then(|content| serde_json::from_str(&content).ok())
    }

    // ---------- 工作流会话 ----------

    pub fn insert_workflow(&self, workflow: WorkflowInsert<'_>) {
        let WorkflowInsert {
            id,
            title,
            state,
            plan,
            project,
            created_at,
        } = workflow;
        let _ = self.workflows.lock().execute(
            "INSERT INTO workflows (id, title, state, plan, project, pinned, unread, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?6)",
            params![id, title, state.as_str(), plan, project, created_at as i64],
        );
    }

    pub fn workflow(&self, id: &str) -> Option<WorkflowRow> {
        self.workflows
            .lock()
            .query_row(
                "SELECT id, title, state, plan, project, pinned, created_at, updated_at, unread FROM workflows WHERE id = ?1",
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
            "SELECT id, title, state, plan, project, pinned, created_at, updated_at, unread FROM workflows
             WHERE (?1 IS NULL OR (?1 = '' AND project IS NULL) OR project = ?1)
             ORDER BY pinned DESC, created_at DESC, id DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![project], row_to_workflow);
        let workflows = collect(rows);
        let mut pinned = Vec::new();
        let mut unpinned = Vec::new();
        for workflow in workflows {
            if workflow.pinned {
                pinned.push(workflow);
            } else {
                unpinned.push(workflow);
            }
        }
        let has_more = unpinned.len() > offset + limit;
        let mut page = pinned;
        page.extend(unpinned.into_iter().skip(offset).take(limit));
        (page, has_more)
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

    /// 更新普通会话模型偏好（docs/DESIGN.md「普通会话存储」）。
    pub fn set_session_model(&self, id: &str, model: &str) {
        if model.trim().is_empty() {
            return;
        }
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET model = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, model, now_ms() as i64],
        );
    }

    /// 更新工作流会话所属项目（None = 未归属）。
    pub fn set_workflow_project(&self, id: &str, project: Option<&str>) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET project = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, project, now_ms() as i64],
        );
    }

    pub fn set_session_pinned(&self, id: &str, pinned: bool) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET pinned = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, pinned, now_ms() as i64],
        );
    }

    pub fn set_session_unread(&self, id: &str, unread: bool) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET unread = ?2 WHERE id = ?1",
            params![id, unread],
        );
    }

    pub fn set_workflow_pinned(&self, id: &str, pinned: bool) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET pinned = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, pinned, now_ms() as i64],
        );
    }

    pub fn set_workflow_unread(&self, id: &str, unread: bool) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET unread = ?2 WHERE id = ?1",
            params![id, unread],
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
        let _ = self.groups.lock().execute(
            "UPDATE groups SET project = NULL WHERE project = ?1",
            params![project],
        );
    }

    pub fn delete_workflow(&self, id: &str) {
        let _ = self
            .workflows
            .lock()
            .execute("DELETE FROM workflows WHERE id = ?1", params![id]);
    }

    pub fn linked_sessions(&self, workflow_id: &str) -> Vec<String> {
        self.sessions_all()
            .into_iter()
            .filter(|session| session.workflow_id.as_deref() == Some(workflow_id))
            .map(|session| session.id)
            .collect()
    }

    // ---------- 群聊会话 ----------

    pub fn group_dir(&self, group_id: &str) -> PathBuf {
        self.home.join("groups").join(group_id)
    }

    pub fn insert_group(&self, id: &str, project: Option<&str>, created_at: u64) {
        let _ = self.groups.lock().execute(
            "INSERT INTO groups (id, title, state, project, pinned, unread, created_at, updated_at)
             VALUES (?1, '', 'idle', ?2, 0, 0, ?3, ?3)",
            params![id, project, created_at as i64],
        );
    }

    pub fn group(&self, id: &str) -> Option<GroupRow> {
        self.groups
            .lock()
            .query_row(
                "SELECT id, title, state, project, pinned, created_at, updated_at, unread FROM groups WHERE id = ?1",
                params![id],
                row_to_group,
            )
            .optional()
            .ok()
            .flatten()
    }

    /// 群聊列表：分页返回非置顶群聊会话，并始终附带该项目全部置顶会话。
    pub fn groups_page(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> (Vec<GroupRow>, bool) {
        let conn = self.groups.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, title, state, project, pinned, created_at, updated_at, unread FROM groups
             WHERE (?1 IS NULL OR (?1 = '' AND project IS NULL) OR project = ?1)
             ORDER BY pinned DESC, created_at DESC, id DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![project], row_to_group);
        let groups = collect(rows);
        let mut pinned = Vec::new();
        let mut unpinned = Vec::new();
        for group in groups {
            if group.pinned {
                pinned.push(group);
            } else {
                unpinned.push(group);
            }
        }
        let has_more = unpinned.len() > offset + limit;
        let mut page = pinned;
        page.extend(unpinned.into_iter().skip(offset).take(limit));
        (page, has_more)
    }

    pub fn set_group_state(&self, id: &str, state: SessionState) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state.as_str(), now_ms() as i64],
        );
    }

    pub fn set_group_title(&self, id: &str, title: &str) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now_ms() as i64],
        );
    }

    pub fn set_group_title_if_empty(&self, id: &str, title: &str) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET title = ?2, updated_at = ?3
             WHERE id = ?1 AND (title IS NULL OR title = '')",
            params![id, title, now_ms() as i64],
        );
    }

    pub fn touch_group(&self, id: &str) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms() as i64],
        );
    }

    pub fn set_group_project(&self, id: &str, project: Option<&str>) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET project = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, project, now_ms() as i64],
        );
    }

    pub fn set_group_pinned(&self, id: &str, pinned: bool) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET pinned = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, pinned, now_ms() as i64],
        );
    }

    pub fn set_group_unread(&self, id: &str, unread: bool) {
        let _ = self.groups.lock().execute(
            "UPDATE groups SET unread = ?2 WHERE id = ?1",
            params![id, unread],
        );
    }

    pub fn delete_group(&self, id: &str) {
        let conn = self.groups.lock();
        let _ = conn.execute("DELETE FROM members WHERE group_id = ?1", params![id]);
        let _ = conn.execute("DELETE FROM groups WHERE id = ?1", params![id]);
        drop(conn);
        self.group_transcripts.lock().remove(id);
        let dir = self.group_dir(id);
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("删除群聊数据目录 {} 失败: {error}", dir.display());
            }
        }
    }

    pub fn insert_group_member(&self, member: &GroupMemberInsert<'_>) {
        let GroupMemberInsert {
            id,
            group_id,
            machine,
            agent,
            session_id,
            name,
            description,
            created_at,
        } = member;
        let _ = self.groups.lock().execute(
            "INSERT INTO members (id, group_id, machine, agent, session_id, name, description, pinned, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8)",
            params![
                id,
                group_id,
                machine,
                agent,
                session_id,
                name,
                description,
                *created_at as i64
            ],
        );
    }

    pub fn group_member(&self, member_id: &str) -> Option<MemberRow> {
        self.groups
            .lock()
            .query_row(
                "SELECT id, group_id, machine, agent, session_id, name, description, pinned, created_at, updated_at
                 FROM members WHERE id = ?1",
                params![member_id],
                row_to_member,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn group_member_by_session(&self, session_id: &str) -> Option<MemberRow> {
        self.groups
            .lock()
            .query_row(
                "SELECT id, group_id, machine, agent, session_id, name, description, pinned, created_at, updated_at
                 FROM members WHERE session_id = ?1",
                params![session_id],
                row_to_member,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn group_members(&self, group_id: &str) -> Vec<MemberRow> {
        let conn = self.groups.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, group_id, machine, agent, session_id, name, description, pinned, created_at, updated_at
             FROM members WHERE group_id = ?1
             ORDER BY pinned DESC, created_at DESC, id DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        collect(stmt.query_map(params![group_id], row_to_member))
    }

    pub fn member_sessions(&self, group_id: &str) -> Vec<String> {
        self.group_members(group_id)
            .into_iter()
            .map(|member| member.session_id)
            .collect()
    }

    /// 追加一条群聊消息（用户消息直接存内容；成员消息存普通会话消息引用）。
    pub fn insert_group_message(
        &self,
        group_id: &str,
        id: &str,
        member_id: Option<&str>,
        session_message_id: Option<&str>,
        user_content: Option<&str>,
        created_at: u64,
    ) {
        let Ok(Some(conn)) = self.group_transcript(group_id, true) else {
            return;
        };
        let conn = conn.lock();
        // ACP 流式消息会以同一 message_id 多次 upsert；群聊只保留一条引用，
        // 展示时始终读取普通会话中的最新完整内容。
        if let (Some(member_id), Some(session_message_id)) = (member_id, session_message_id) {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM messages
                        WHERE group_id = ?1 AND member_id = ?2 AND session_message_id = ?3
                     )",
                    params![group_id, member_id, session_message_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if exists {
                return;
            }
        }
        let _ = conn.execute(
            "INSERT OR IGNORE INTO messages (id, group_id, member_id, session_message_id, user_content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                id,
                group_id,
                member_id,
                session_message_id,
                user_content,
                created_at as i64
            ],
        );
    }

    /// 获取群聊 transcript 连接；与普通会话 transcript 一样按群聊缓存。
    fn group_transcript(
        &self,
        group_id: &str,
        create: bool,
    ) -> Result<Option<Arc<Mutex<Connection>>>, String> {
        let mut transcripts = self.group_transcripts.lock();
        if let Some(transcript) = transcripts.get(group_id) {
            return Ok(Some(transcript));
        }
        let path = self.group_dir(group_id).join("transcript.sqlite");
        if !create && !path.is_file() {
            return Ok(None);
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("群聊 transcript 路径无效: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败: {error}", parent.display()))?;
        let transcript = Arc::new(Mutex::new(open_db(&path, GROUP_TRANSCRIPT_SCHEMA)?));
        transcripts.insert(group_id.to_string(), Arc::clone(&transcript));
        Ok(Some(transcript))
    }

    /// 群聊消息分页（最新优先，返回前按时间正序）。
    fn group_message_rows(
        &self,
        group_id: &str,
        limit: usize,
        offset: usize,
    ) -> (Vec<GroupMessageRow>, bool) {
        let Some(conn) = self.group_transcript(group_id, false).ok().flatten() else {
            return (Vec::new(), false);
        };
        let conn = conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, member_id, session_message_id, user_content, created_at FROM messages
             ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![(limit + 1) as i64, offset as i64], |row| {
            Ok(GroupMessageRow {
                id: row.get(0)?,
                member_id: row.get(1)?,
                session_message_id: row.get(2)?,
                user_content: row.get(3)?,
                created_at: row.get::<_, i64>(4)? as u64,
            })
        });
        let mut items = match rows {
            Ok(rows) => rows.flatten().collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        let has_more = items.len() > limit;
        items.truncate(limit);
        items.reverse();
        (items, has_more)
    }

    /// 某时间之后的全部群聊消息（按时间正序）。
    pub fn group_messages_since(&self, group_id: &str, after_ms: u64) -> Vec<GroupMessageRow> {
        let Some(conn) = self.group_transcript(group_id, false).ok().flatten() else {
            return Vec::new();
        };
        let conn = conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, member_id, session_message_id, user_content, created_at FROM messages
             WHERE created_at > ?1 ORDER BY created_at ASC, id ASC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![after_ms as i64], |row| {
            Ok(GroupMessageRow {
                id: row.get(0)?,
                member_id: row.get(1)?,
                session_message_id: row.get(2)?,
                user_content: row.get(3)?,
                created_at: row.get::<_, i64>(4)? as u64,
            })
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 成员普通会话上一次用户消息时刻；从未驱动过时取会话创建时间。
    pub fn last_user_message_at(&self, session_id: &str, fallback: u64) -> u64 {
        let Some(transcript) = self.transcript(session_id, false).ok().flatten() else {
            return fallback;
        };
        let at: Option<i64> = transcript
            .lock()
            .query_row(
                "SELECT MAX(created_at) FROM messages WHERE role = 'user'",
                [],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        at.map(|at| at as u64).unwrap_or(fallback)
    }

    /// 按普通会话消息 ID 读取消息内容块。
    pub fn session_message(&self, session_id: &str, message_id: &str) -> Option<Vec<ContentBlock>> {
        let transcript = self.transcript(session_id, false).ok()??;
        let content = transcript
            .lock()
            .query_row(
                "SELECT content FROM messages WHERE message_id = ?1",
                params![message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()?;
        serde_json::from_str(&content).ok()
    }

    /// 群聊历史分页：解析成员消息引用为完整内容。
    pub fn group_messages_page(
        &self,
        group_id: &str,
        limit: usize,
        offset: usize,
    ) -> (Vec<GroupMessage>, bool) {
        let (rows, has_more) = self.group_message_rows(group_id, limit, offset);
        let members: HashMap<String, MemberRow> = self
            .group_members(group_id)
            .into_iter()
            .map(|member| (member.id.clone(), member))
            .collect();
        let messages = rows
            .into_iter()
            .filter_map(|row| {
                let member = row.member_id.as_ref().and_then(|id| members.get(id));
                match member {
                    Some(member) => {
                        let session_message_id = row.session_message_id?;
                        let content =
                            self.session_message(&member.session_id, &session_message_id)?;
                        Some(GroupMessage {
                            id: row.id,
                            member_id: Some(member.id.clone()),
                            name: member.name.clone(),
                            agent: format!("{}@{}", member.agent, member.machine),
                            description: member.description.clone(),
                            content,
                            timestamp: row.created_at,
                        })
                    }
                    None => {
                        let content: Vec<ContentBlock> =
                            serde_json::from_str(row.user_content.as_deref()?).ok()?;
                        Some(GroupMessage {
                            id: row.id,
                            member_id: None,
                            name: "user".to_string(),
                            agent: String::new(),
                            description: String::new(),
                            content,
                            timestamp: row.created_at,
                        })
                    }
                }
            })
            .collect();
        (messages, has_more)
    }

    // ---------- 自动化 ----------

    pub fn automations(&self) -> Vec<AutomationRow> {
        let conn = self.automations.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, title, enabled, task, trigger, session_options,
                    result_labels, created_at, updated_at
             FROM automations ORDER BY created_at DESC, id DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        collect(stmt.query_map([], row_to_automation_record))
            .into_iter()
            .filter_map(AutomationRecord::decode)
            .collect()
    }

    pub fn automation(&self, id: &str) -> Option<AutomationRow> {
        let record = self
            .automations
            .lock()
            .query_row(
                "SELECT id, title, enabled, task, trigger, session_options,
                        result_labels, created_at, updated_at
                 FROM automations WHERE id = ?1",
                params![id],
                row_to_automation_record,
            )
            .optional()
            .ok()
            .flatten()?;
        record.decode()
    }

    pub fn insert_automation(
        &self,
        id: &str,
        input: &AutomationInput,
        created_at: u64,
        updated_at: u64,
    ) {
        let trigger = serde_json::to_string(&input.trigger).unwrap_or_default();
        let options = serde_json::to_string(&input.session_options).unwrap_or_default();
        let labels = serde_json::to_string(&input.result_labels).unwrap_or_default();
        let _ = self.automations.lock().execute(
            "INSERT INTO automations (
                id, title, enabled, task, trigger, session_options,
                result_labels, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                input.title,
                input.enabled,
                input.task,
                trigger,
                options,
                labels,
                created_at as i64,
                updated_at as i64,
            ],
        );
    }

    pub fn update_automation(&self, id: &str, input: &AutomationInput, updated_at: u64) -> bool {
        let trigger = serde_json::to_string(&input.trigger).unwrap_or_default();
        let options = serde_json::to_string(&input.session_options).unwrap_or_default();
        let labels = serde_json::to_string(&input.result_labels).unwrap_or_default();
        self.automations
            .lock()
            .execute(
                "UPDATE automations
                 SET title = ?2, enabled = ?3, task = ?4, trigger = ?5,
                     session_options = ?6, result_labels = ?7, updated_at = ?8
                 WHERE id = ?1",
                params![
                    id,
                    input.title,
                    input.enabled,
                    input.task,
                    trigger,
                    options,
                    labels,
                    updated_at as i64,
                ],
            )
            .is_ok_and(|changed| changed > 0)
    }

    pub fn delete_automation(&self, id: &str) -> bool {
        let conn = self.automations.lock();
        let deleted = conn
            .execute("DELETE FROM automations WHERE id = ?1", params![id])
            .is_ok_and(|changed| changed > 0);
        if deleted {
            let _ = conn.execute("DELETE FROM runs WHERE automation_id = ?1", params![id]);
        }
        deleted
    }

    pub fn insert_automation_run(&self, run: &AutomationRun) {
        let _ = self.automations.lock().execute(
            "INSERT INTO runs (
                id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.id,
                run.automation_id,
                run.session_id,
                run.workflow_id,
                run.group_id,
                run.state.as_str(),
                run.error,
                run.result_label,
                run.created_at as i64,
                run.started_at.map(|value| value as i64),
                run.finished_at.map(|value| value as i64),
            ],
        );
    }

    pub fn start_automation_run(
        &self,
        run_id: &str,
        session_id: Option<&str>,
        workflow_id: Option<&str>,
        group_id: Option<&str>,
        started_at: u64,
    ) {
        let _ = self.automations.lock().execute(
            "UPDATE runs
             SET state = 'running', session_id = ?2, workflow_id = ?3, group_id = ?4,
                 started_at = ?5
             WHERE id = ?1 AND state = 'queued'",
            params![run_id, session_id, workflow_id, group_id, started_at as i64],
        );
    }

    pub fn finish_automation_run(&self, run_id: &str, finished_at: u64) {
        let _ = self.automations.lock().execute(
            "UPDATE runs
             SET state = 'finished', error = NULL, finished_at = ?2
             WHERE id = ?1 AND state = 'running'",
            params![run_id, finished_at as i64],
        );
    }

    pub fn fail_automation_run(&self, run_id: &str, error: &str, finished_at: u64) {
        let _ = self.automations.lock().execute(
            "UPDATE runs
             SET state = 'failed', error = ?2, finished_at = ?3
             WHERE id = ?1 AND state IN ('queued', 'running')",
            params![run_id, error, finished_at as i64],
        );
    }

    pub fn set_automation_run_label(
        &self,
        run_id: &str,
        result_label: Option<&str>,
        error: Option<&str>,
    ) {
        let _ = self.automations.lock().execute(
            "UPDATE runs SET result_label = ?2, error = ?3 WHERE id = ?1 AND state = 'finished'",
            params![run_id, result_label, error],
        );
    }

    pub fn automation_run(&self, id: &str) -> Option<AutomationRun> {
        let record = self
            .automations
            .lock()
            .query_row(
                "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                        created_at, started_at, finished_at
                 FROM runs WHERE id = ?1",
                params![id],
                row_to_automation_run,
            )
            .optional()
            .ok()
            .flatten()?;
        record.decode()
    }

    pub fn latest_automation_run(&self, automation_id: &str) -> Option<AutomationRun> {
        let record = self
            .automations
            .lock()
            .query_row(
                "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                        created_at, started_at, finished_at
                 FROM runs WHERE automation_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![automation_id],
                row_to_automation_run,
            )
            .optional()
            .ok()
            .flatten()?;
        record.decode()
    }

    pub fn running_automation_run_for_session(&self, session_id: &str) -> Option<AutomationRun> {
        let record = self
            .automations
            .lock()
            .query_row(
                "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                        created_at, started_at, finished_at
                 FROM runs
                 WHERE session_id = ?1 AND state = 'running'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![session_id],
                row_to_automation_run,
            )
            .optional()
            .ok()
            .flatten()?;
        record.decode()
    }

    pub fn running_automation_run_for_workflow(&self, workflow_id: &str) -> Option<AutomationRun> {
        let record = self
            .automations
            .lock()
            .query_row(
                "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                        created_at, started_at, finished_at
                 FROM runs
                 WHERE workflow_id = ?1 AND state = 'running'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![workflow_id],
                row_to_automation_run,
            )
            .optional()
            .ok()
            .flatten()?;
        record.decode()
    }

    pub fn running_automation_run_for_group(&self, group_id: &str) -> Option<AutomationRun> {
        let record = self
            .automations
            .lock()
            .query_row(
                "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                        created_at, started_at, finished_at
                 FROM runs
                 WHERE group_id = ?1 AND state = 'running'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![group_id],
                row_to_automation_run,
            )
            .optional()
            .ok()
            .flatten()?;
        record.decode()
    }

    pub fn running_automation_runs(&self) -> Vec<AutomationRun> {
        let conn = self.automations.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                    created_at, started_at, finished_at
             FROM runs WHERE state = 'running'
             ORDER BY created_at ASC, id ASC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        collect(stmt.query_map([], row_to_automation_run))
            .into_iter()
            .filter_map(AutomationRunRecord::decode)
            .collect()
    }

    pub fn automation_runs_page(
        &self,
        automation_id: &str,
        limit: usize,
        offset: usize,
    ) -> (Vec<AutomationRun>, bool) {
        let conn = self.automations.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, automation_id, session_id, workflow_id, group_id, state, error, result_label,
                    created_at, started_at, finished_at
             FROM runs WHERE automation_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(
            params![automation_id, (limit + 1) as i64, offset as i64],
            row_to_automation_run,
        );
        let mut runs: Vec<AutomationRun> = collect(rows)
            .into_iter()
            .filter_map(AutomationRunRecord::decode)
            .collect();
        let has_more = runs.len() > limit;
        runs.truncate(limit);
        (runs, has_more)
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
    pub pinned: bool,
    pub unread: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 群聊会话元数据行。
#[derive(Debug, Clone)]
pub struct GroupRow {
    pub id: String,
    pub title: String,
    pub state: SessionState,
    pub project: Option<String>,
    pub pinned: bool,
    pub unread: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 群聊成员行。
#[derive(Debug, Clone)]
pub struct MemberRow {
    pub id: String,
    pub group_id: String,
    pub machine: String,
    pub agent: String,
    pub session_id: String,
    pub name: String,
    pub description: String,
    pub pinned: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 新建群聊成员所需输入。
pub struct GroupMemberInsert<'a> {
    pub id: &'a str,
    pub group_id: &'a str,
    pub machine: &'a str,
    pub agent: &'a str,
    pub session_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub created_at: u64,
}

/// 群聊 transcript 消息行：成员消息经引用指向成员普通会话数据。
#[derive(Debug, Clone)]
pub struct GroupMessageRow {
    pub id: String,
    pub member_id: Option<String>,
    pub session_message_id: Option<String>,
    pub user_content: Option<String>,
    pub created_at: u64,
}

/// 自动化元数据行；JSON 列在读取时解码成强类型字段。
#[derive(Debug, Clone)]
pub struct AutomationRow {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub task: String,
    pub trigger: AutomationTrigger,
    pub session_options: AutomationSessionOptions,
    pub result_labels: Vec<AutomationResultLabel>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug)]
struct AutomationRecord {
    id: String,
    title: String,
    enabled: bool,
    task: String,
    trigger: String,
    session_options: String,
    result_labels: String,
    created_at: u64,
    updated_at: u64,
}

impl AutomationRecord {
    fn decode(self) -> Option<AutomationRow> {
        Some(AutomationRow {
            id: self.id,
            title: self.title,
            enabled: self.enabled,
            task: self.task,
            trigger: serde_json::from_str(&self.trigger).ok()?,
            session_options: serde_json::from_str(&self.session_options).ok()?,
            result_labels: serde_json::from_str(&self.result_labels).ok()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(Debug)]
struct AutomationRunRecord {
    id: String,
    automation_id: String,
    session_id: Option<String>,
    workflow_id: Option<String>,
    group_id: Option<String>,
    state: String,
    error: Option<String>,
    result_label: Option<String>,
    created_at: u64,
    started_at: Option<u64>,
    finished_at: Option<u64>,
}

impl AutomationRunRecord {
    fn decode(self) -> Option<AutomationRun> {
        Some(AutomationRun {
            id: self.id,
            automation_id: self.automation_id,
            session_id: self.session_id,
            workflow_id: self.workflow_id,
            group_id: self.group_id,
            state: parse_automation_run_state(&self.state)?,
            error: self.error,
            result_label: self.result_label,
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
        })
    }
}

/// 中心 `session.sqlite` 表结构。
const SESSION_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        workflow_id TEXT,
        group_id TEXT,
        state TEXT NOT NULL,
        title TEXT,
        project TEXT,
        pinned BOOL NOT NULL,
        unread BOOL NOT NULL,
        workspace TEXT NOT NULL,
        worktree_dir TEXT,
        machine TEXT NOT NULL,
        agent TEXT NOT NULL,
        model TEXT,
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
        pinned BOOL NOT NULL,
        unread BOOL NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );";

/// `group.sqlite` 表结构（docs/DESIGN.md「群聊会话存储」）。
const GROUP_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS groups (
        id TEXT PRIMARY KEY,
        title TEXT,
        state TEXT NOT NULL,
        project TEXT,
        pinned BOOL NOT NULL,
        unread BOOL NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS members (
        id TEXT PRIMARY KEY,
        group_id TEXT NOT NULL,
        machine TEXT NOT NULL,
        agent TEXT NOT NULL,
        session_id TEXT NOT NULL,
        name TEXT NOT NULL,
        description TEXT NOT NULL,
        pinned BOOL NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        CONSTRAINT uq_group_id_name UNIQUE (group_id, name)
    );";

/// 每群聊 `transcript.sqlite` 表结构（docs/DESIGN.md「群聊会话存储」）。
const GROUP_TRANSCRIPT_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        group_id TEXT NOT NULL,
        member_id TEXT,
        session_message_id TEXT,
        user_content TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );";

/// `automation.sqlite` 表结构（独立数据库按 docs/DESIGN.md「自动化存储」）。
const AUTOMATION_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS automations (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        enabled BOOL NOT NULL,
        task TEXT NOT NULL,
        trigger TEXT NOT NULL,
        session_options TEXT NOT NULL,
        result_labels TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS runs (
        id TEXT PRIMARY KEY,
        automation_id TEXT NOT NULL,
        session_id TEXT,
        workflow_id TEXT,
        group_id TEXT,
        state TEXT NOT NULL,
        error TEXT,
        result_label TEXT,
        created_at INTEGER NOT NULL,
        started_at INTEGER,
        finished_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS runs_by_automation
        ON runs(automation_id, created_at DESC);";

fn open_db(path: &Path, schema: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
    conn.execute_batch(&format!("PRAGMA journal_mode = WAL;\n{schema}"))
        .map_err(|e| format!("初始化表失败: {e}"))?;
    Ok(conn)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        group_id: row.get(2)?,
        state: amux_common::domain::parse_session_state(row.get::<_, String>(3)?.as_str())
            .unwrap_or(SessionState::Idle),
        title: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        project: row.get(5)?,
        pinned: row.get(6)?,
        unread: row.get(7)?,
        workspace: row.get(8)?,
        worktree_dir: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        machine: row.get(10)?,
        agent: row.get(11)?,
        model: row.get(12)?,
        created_at: row.get::<_, i64>(14)? as u64,
        updated_at: row.get::<_, i64>(15)? as u64,
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
        pinned: row.get(5)?,
        created_at: row.get::<_, i64>(6)? as u64,
        updated_at: row.get::<_, i64>(7)? as u64,
        unread: row.get(8)?,
    })
}

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRow> {
    Ok(GroupRow {
        id: row.get(0)?,
        title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        state: amux_common::domain::parse_session_state(row.get::<_, String>(2)?.as_str())
            .unwrap_or(SessionState::Idle),
        project: row.get(3)?,
        pinned: row.get(4)?,
        created_at: row.get::<_, i64>(5)? as u64,
        updated_at: row.get::<_, i64>(6)? as u64,
        unread: row.get(7)?,
    })
}

fn row_to_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemberRow> {
    Ok(MemberRow {
        id: row.get(0)?,
        group_id: row.get(1)?,
        machine: row.get(2)?,
        agent: row.get(3)?,
        session_id: row.get(4)?,
        name: row.get(5)?,
        description: row.get(6)?,
        pinned: row.get(7)?,
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
    })
}

fn row_to_automation_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutomationRecord> {
    Ok(AutomationRecord {
        id: row.get(0)?,
        title: row.get(1)?,
        enabled: row.get(2)?,
        task: row.get(3)?,
        trigger: row.get(4)?,
        session_options: row.get(5)?,
        result_labels: row.get(6)?,
        created_at: row.get::<_, i64>(7)? as u64,
        updated_at: row.get::<_, i64>(8)? as u64,
    })
}

fn row_to_automation_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutomationRunRecord> {
    Ok(AutomationRunRecord {
        id: row.get(0)?,
        automation_id: row.get(1)?,
        session_id: row.get(2)?,
        workflow_id: row.get(3)?,
        group_id: row.get(4)?,
        state: row.get(5)?,
        error: row.get(6)?,
        result_label: row.get(7)?,
        created_at: row.get::<_, i64>(8)? as u64,
        started_at: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
        finished_at: row.get::<_, Option<i64>>(10)?.map(|value| value as u64),
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
            workflow_id: None,
            group_id: None,
            machine: machine.into(),
            agent: agent.into(),
            model: None,
            title: String::new(),
            state: SessionState::Idle,
            project: None,
            pinned: false,
            unread: false,
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
        store.insert_workflow(WorkflowInsert {
            id: "w1",
            title: "wf",
            state: SessionState::Idle,
            plan: "plan",
            project: None,
            created_at: 1,
        });
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
    fn session_model_round_trips_and_can_be_updated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut row = session("s1", "pc", "codex");
        row.model = Some("gpt-5".into());
        store.insert_session(&row).unwrap();
        assert_eq!(store.session("s1").unwrap().model.as_deref(), Some("gpt-5"));

        store.set_session_model("s1", "gpt-4o");
        assert_eq!(
            store.session("s1").unwrap().model.as_deref(),
            Some("gpt-4o")
        );
    }

    #[test]
    fn agent_session_id_can_only_be_claimed_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();

        assert!(store.claim_agent_session_id("s1", "agent-1"));
        assert!(!store.claim_agent_session_id("s1", "agent-2"));
        assert_eq!(store.agent_session_id("s1").as_deref(), Some("agent-1"));
    }

    #[test]
    fn sessions_page_includes_all_pinned_and_paginates_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, created_at, pinned) in [
            ("s1", 70u64, false),
            ("s2", 80, false),
            ("p-old", 50, true),
            ("p-new", 100, true),
        ] {
            let mut row = session(id, "pc", "codex");
            row.created_at = created_at;
            row.updated_at = created_at;
            store.insert_session(&row).unwrap();
            if pinned {
                store.set_session_pinned(id, true);
            }
        }

        let (first, has_more) = store.sessions_page(1, 0, None);
        assert_eq!(
            first.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["p-new", "p-old", "s2"]
        );
        assert!(has_more);

        let (second, has_more) = store.sessions_page(1, 1, None);
        assert_eq!(
            second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["p-new", "p-old", "s1"]
        );
        assert!(!has_more);
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
    fn linked_sessions_sort_pinned_before_session_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, created_at) in [("s1", 100u64), ("s2", 300), ("s3", 200)] {
            let mut s = session(id, "pc", "codex");
            s.created_at = created_at;
            store.insert_session(&s).unwrap();
        }
        store.insert_workflow(WorkflowInsert {
            id: "w1",
            title: "wf",
            state: SessionState::Idle,
            plan: "plan",
            project: None,
            created_at: 1,
        });
        // 按任意顺序关联，返回时置顶优先，其余按创建时间倒序
        store.link_session("w1", "s3");
        store.link_session("w1", "s1");
        store.link_session("w1", "s2");
        store.set_session_pinned("s2", true);

        assert_eq!(store.linked_sessions("w1"), ["s2", "s3", "s1"]);
    }

    #[test]
    fn workflows_page_filters_project_before_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, plan, project, created_at) in [
            ("a1", "p", "p", 10),
            ("b1", "q", "q", 9),
            ("a2", "p", "p", 8),
        ] {
            store.insert_workflow(WorkflowInsert {
                id,
                title: id,
                state: SessionState::Idle,
                plan,
                project: Some(project),
                created_at,
            });
        }

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
        store.insert_workflow(WorkflowInsert {
            id: "a1",
            title: "a1",
            state: SessionState::Idle,
            plan: "p",
            project: None,
            created_at: 10,
        });
        store.insert_workflow(WorkflowInsert {
            id: "b1",
            title: "b1",
            state: SessionState::Idle,
            plan: "q",
            project: Some("q"),
            created_at: 9,
        });
        store.insert_workflow(WorkflowInsert {
            id: "a2",
            title: "a2",
            state: SessionState::Idle,
            plan: "p",
            project: None,
            created_at: 8,
        });

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
    fn workflows_page_includes_all_pinned_and_paginates_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, created_at) in [("w1", 70), ("w2", 80), ("pinned", 100)] {
            store.insert_workflow(WorkflowInsert {
                id,
                title: id,
                state: SessionState::Idle,
                plan: "p",
                project: None,
                created_at,
            });
        }
        store.set_workflow_pinned("pinned", true);

        let (first, has_more) = store.workflows_page(1, 0, None);
        assert_eq!(
            first.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["pinned", "w2"]
        );
        assert!(has_more);

        let (second, has_more) = store.workflows_page(1, 1, None);
        assert_eq!(
            second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["pinned", "w1"]
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
    fn automation_and_runs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let input = AutomationInput {
            title: "每日任务".into(),
            enabled: true,
            task: "执行检查".into(),
            trigger: AutomationTrigger::Schedule {
                cron: "0 9 * * *".into(),
            },
            session_options: AutomationSessionOptions::NewSession {
                machine: "pc".into(),
                agent: "codex".into(),
                model: None,
                workspace: "/repo".into(),
                use_worktree: false,
                worktree_base: None,
                project: Some("p".into()),
            },
            result_labels: vec![
                AutomationResultLabel {
                    name: "通过".into(),
                    description: "已完成".into(),
                },
                AutomationResultLabel {
                    name: "失败".into(),
                    description: "未完成".into(),
                },
            ],
        };
        store.insert_automation("a1", &input, 10, 10);
        store.insert_automation_run(&AutomationRun {
            id: "r1".into(),
            automation_id: "a1".into(),
            session_id: None,
            workflow_id: None,
            group_id: None,
            state: amux_common::api::AutomationRunState::Queued,
            result_label: None,
            error: None,
            created_at: 11,
            started_at: None,
            finished_at: None,
        });
        store.start_automation_run("r1", None, Some("w1"), None, 11);
        store.finish_automation_run("r1", 12);
        store.set_automation_run_label("r1", Some("通过"), None);

        let automation = store.automation("a1").unwrap();
        assert_eq!(automation.title, "每日任务");
        assert_eq!(automation.result_labels.len(), 2);
        let run = store.latest_automation_run("a1").unwrap();
        assert_eq!(run.state, amux_common::api::AutomationRunState::Finished);
        assert_eq!(run.workflow_id.as_deref(), Some("w1"));
        assert_eq!(run.started_at, Some(11));
        assert_eq!(run.result_label.as_deref(), Some("通过"));
        assert_eq!(run.finished_at, Some(12));
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

    #[test]
    fn streamed_updates_merge_with_persisted_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        let text = |value: &str| {
            ContentBlock::Text(agent_client_protocol::schema::v2::TextContent::new(value))
        };

        store.update_message("s1", "m1", "agent", 1, |_| vec![text("he")]);
        store.update_message("s1", "m1", "agent", 2, |current| {
            let mut blocks = current.unwrap();
            blocks.push(text("llo"));
            blocks
        });
        assert_eq!(
            store.session_message("s1", "m1"),
            Some(vec![text("he"), text("llo")])
        );

        store.update_message("s1", "m1", "agent", 3, |_| vec![text("snapshot")]);
        assert_eq!(
            store.session_message("s1", "m1"),
            Some(vec![text("snapshot")])
        );

        store.update_activity("s1", "think-1", "thinking", 1, |_| Activity::Thinking {
            id: "think-1".into(),
            timestamp: 1,
            thinking: "he".into(),
        });
        store.update_activity("s1", "think-1", "thinking", 2, |current| {
            let Some(Activity::Thinking { thinking, .. }) = current else {
                panic!("应读取到磁盘中的思考活动");
            };
            Activity::Thinking {
                id: "think-1".into(),
                timestamp: 2,
                thinking: format!("{thinking}llo"),
            }
        });
        assert_eq!(
            store.activity_by_id("s1", "think-1"),
            Some(Activity::Thinking {
                id: "think-1".into(),
                timestamp: 2,
                thinking: "hello".into(),
            })
        );
    }

    #[test]
    fn sessions_page_excludes_group_member_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_session(&session("s2", "pc", "codex")).unwrap();
        store.insert_group("g1", None, 1);
        store.link_group_session("g1", "s2");

        let (page, _) = store.sessions_page(10, 0, None);
        assert_eq!(
            page.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["s1"]
        );
        assert_eq!(store.group_of_session("s2").as_deref(), Some("g1"));
    }

    #[test]
    fn unread_flags_round_trip_for_all_session_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_workflow(WorkflowInsert {
            id: "w1",
            title: "workflow",
            state: SessionState::Idle,
            plan: "plan",
            project: None,
            created_at: 1,
        });
        store.insert_group("g1", None, 1);

        store.set_session_unread("s1", true);
        store.set_workflow_unread("w1", true);
        store.set_group_unread("g1", true);
        assert!(store.session("s1").unwrap().unread);
        assert!(store.workflow("w1").unwrap().unread);
        assert!(store.group("g1").unwrap().unread);

        store.set_session_unread("s1", false);
        store.set_workflow_unread("w1", false);
        store.set_group_unread("g1", false);
        assert!(!store.session("s1").unwrap().unread);
        assert!(!store.workflow("w1").unwrap().unread);
        assert!(!store.group("g1").unwrap().unread);
    }

    #[test]
    fn group_member_names_are_unique_within_group() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_group("g1", None, 1);
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_session(&session("s2", "pc", "codex")).unwrap();
        for (id, session_id) in [("m1", "s1"), ("m2", "s2")] {
            store.insert_group_member(&GroupMemberInsert {
                id,
                group_id: "g1",
                machine: "pc",
                agent: "codex",
                session_id,
                name: "coder",
                description: "",
                created_at: 1,
            });
        }
        assert_eq!(
            store.group_members("g1").len(),
            1,
            "同名成员写入应被唯一约束拒绝"
        );
    }

    #[test]
    fn group_history_resolves_member_session_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_group("g1", None, 1);
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_group_member(&GroupMemberInsert {
            id: "m1",
            group_id: "g1",
            machine: "pc",
            agent: "codex",
            session_id: "s1",
            name: "coder",
            description: "负责实现",
            created_at: 1,
        });
        store.upsert_message(
            "s1",
            "agent-message",
            "agent",
            r#"[{"type":"text","text":"实现完成"}]"#,
            20,
        );
        store.insert_group_message(
            "g1",
            "u1",
            None,
            None,
            Some(r#"[{"type":"text","text":"@coder 开始"}]"#),
            10,
        );
        store.insert_group_message("g1", "a1", Some("m1"), Some("agent-message"), None, 20);

        let (messages, has_more) = store.group_messages_page("g1", 10, 0);
        assert!(!has_more);
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.name.as_str(), message.agent.as_str()))
                .collect::<Vec<_>>(),
            [("user", ""), ("coder", "codex@pc")]
        );
        assert_eq!(
            messages[1].content,
            vec![ContentBlock::Text(
                agent_client_protocol::schema::v2::TextContent::new("实现完成")
            )]
        );
    }

    #[test]
    fn group_history_keeps_one_reference_per_streamed_agent_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_group("g1", None, 1);
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_group_member(&GroupMemberInsert {
            id: "m1",
            group_id: "g1",
            machine: "pc",
            agent: "codex",
            session_id: "s1",
            name: "coder",
            description: "负责实现",
            created_at: 1,
        });

        for (message_id, text, at) in [("a1", "实现", 10), ("a1", "实现完成", 11)] {
            store.upsert_message(
                "s1",
                message_id,
                "agent",
                &serde_json::to_string(&vec![ContentBlock::Text(
                    agent_client_protocol::schema::v2::TextContent::new(text),
                )])
                .unwrap(),
                at,
            );
            store.insert_group_message(
                "g1",
                &format!("group-{at}"),
                Some("m1"),
                Some(message_id),
                None,
                at,
            );
        }

        let (messages, _) = store.group_messages_page("g1", 10, 0);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].content,
            vec![ContentBlock::Text(
                agent_client_protocol::schema::v2::TextContent::new("实现完成")
            )]
        );
    }

    #[test]
    fn group_busy_state_is_reset_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_group("g1", None, 1);
        store.set_group_state("g1", SessionState::Busy);
        drop(store);

        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(
            reopened.group("g1").unwrap().state,
            SessionState::Idle,
            "Server 重启后不应残留群聊工作中状态"
        );
    }
}
