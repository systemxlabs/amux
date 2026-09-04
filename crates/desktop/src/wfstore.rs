//! 工作流会话存储：
//! - 元数据：`~/.amux/app/session.sqlite`
//!   - 工作流会话表：标题/状态/执行计划等
//!   - 关联普通会话表：工作流会话 ↔ 关联普通会话（会话 ID + 机器名称）
//! - 对话历史：`~/.amux/app/sessions/<session_id>_history.jsonl`
//! - 活动历史：`~/.amux/app/sessions/<session_id>_activities.jsonl`
//!
//! 历史/活动文件布局与读取复用 `amux-common::session_log`，由引擎在条目
//! 完整后逐条追加写盘。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

#[cfg(test)]
use amux_common::session_log::append_jsonl;
use amux_common::session_log::{activities_path, history_path, read_jsonl};
use protocol::{Activity, ContentBlock, HistoryItem};
use rusqlite::{params, Connection};

use crate::workflow::{LinkedSession, OrcMsg, OrcSession};

const META_SELECT_COLUMNS: &str =
    "id, title, state, last_active_at, description, plan, preamble, created_at, updated_at";

fn meta_select(suffix: &str) -> String {
    format!("SELECT {META_SELECT_COLUMNS} FROM sessions {suffix}")
}

fn sqlite_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("session.sqlite")
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn transcript_from_history(items: &[HistoryItem]) -> Vec<OrcMsg> {
    items
        .iter()
        .map(|h| match h {
            HistoryItem::UserMessage { content, timestamp } => OrcMsg::User {
                text: content_text(content),
                timestamp: *timestamp,
            },
            HistoryItem::AgentMessage { content, timestamp } => OrcMsg::Orc {
                text: content_text(content),
                timestamp: *timestamp,
            },
        })
        .collect()
}

fn open_db(data_dir: &Path) -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let conn = Connection::open(sqlite_path(data_dir))?;
    // 多个写者可能并发持久化（各自独立连接）：等锁而非报 "database is locked"
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            state TEXT NOT NULL,
            last_active_at INTEGER NOT NULL,
            description TEXT NOT NULL,
            plan TEXT NOT NULL,
            preamble TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS workflow_linked_sessions (
            workflow_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            machine_name TEXT NOT NULL,
            PRIMARY KEY (workflow_id, machine_name, session_id)
        );",
    )?;
    Ok(conn)
}

/// sqlite 中的状态为协议 snake_case 表示；解析统一走 protocol，
/// 未知值报错（坏数据显式失败，不静默回退）。
fn state_from(s: &str) -> io::Result<protocol::SessionState> {
    protocol::parse_session_state(s)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("未知工作流状态: {s}")))
}

struct MetaRow {
    id: String,
    title: String,
    state: String,
    last_active_at: u64,
    description: String,
    plan: String,
    preamble: String,
    created_at: u64,
    updated_at: u64,
}

fn read_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MetaRow> {
    Ok(MetaRow {
        id: row.get(0)?,
        title: row.get(1)?,
        state: row.get(2)?,
        last_active_at: row.get::<_, i64>(3)? as u64,
        description: row.get(4)?,
        plan: row.get(5)?,
        preamble: row.get(6)?,
        created_at: row.get::<_, i64>(7)? as u64,
        updated_at: row.get::<_, i64>(8)? as u64,
    })
}

fn meta_row_to_session(row: rusqlite::Result<MetaRow>) -> io::Result<OrcSession> {
    let row = row.map_err(io::Error::other)?;
    Ok(OrcSession {
        id: row.id,
        title: row.title,
        plan: row.plan,
        description: row.description,
        preamble: row.preamble,
        state: state_from(&row.state)?,
        transcript: Vec::new(),
        linked_sessions: Vec::new(),
        activities: Vec::new(),
        created_at: row.created_at,
        updated_at: row.last_active_at.max(row.updated_at),
    })
}

/// 读取全部关联关系并按工作流分组（关联行很小，一次性读取足够）。
fn load_linked_sessions(
    conn: &Connection,
) -> rusqlite::Result<HashMap<String, Vec<LinkedSession>>> {
    let mut stmt = conn.prepare(
        "SELECT workflow_id, machine_name, session_id
         FROM workflow_linked_sessions
         ORDER BY rowid",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            LinkedSession {
                machine_name: row.get(1)?,
                id: row.get(2)?,
            },
        ))
    })?;
    let mut grouped: HashMap<String, Vec<LinkedSession>> = HashMap::new();
    for row in rows {
        let (workflow_id, linked) = row?;
        grouped.entry(workflow_id).or_default().push(linked);
    }
    Ok(grouped)
}

fn attach_linked_sessions(
    sessions: &mut [OrcSession],
    linked: HashMap<String, Vec<LinkedSession>>,
) {
    for session in sessions {
        if let Some(list) = linked.get(&session.id) {
            session.linked_sessions = list.clone();
        }
    }
}

pub fn save(data_dir: &Path, session: &OrcSession) -> io::Result<()> {
    let mut conn = open_db(data_dir).map_err(io::Error::other)?;
    let tx = conn.transaction().map_err(io::Error::other)?;
    tx.execute(
        "INSERT INTO sessions
            (id, title, state, last_active_at, description, plan, preamble,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
            title=excluded.title, state=excluded.state,
            last_active_at=excluded.last_active_at,
            description=excluded.description, plan=excluded.plan,
            preamble=excluded.preamble,
            created_at=excluded.created_at, updated_at=excluded.updated_at",
        params![
            session.id,
            session.title,
            session.state.as_str(),
            session.updated_at as i64,
            session.description,
            session.plan,
            session.preamble,
            session.created_at as i64,
            session.updated_at as i64,
        ],
    )
    .map_err(io::Error::other)?;
    // 关联关系整组替换：以引擎内存为权威
    tx.execute(
        "DELETE FROM workflow_linked_sessions WHERE workflow_id = ?1",
        params![session.id],
    )
    .map_err(io::Error::other)?;
    for linked in &session.linked_sessions {
        tx.execute(
            "INSERT OR IGNORE INTO workflow_linked_sessions
                (workflow_id, session_id, machine_name)
             VALUES (?1, ?2, ?3)",
            params![session.id, linked.id, linked.machine_name],
        )
        .map_err(io::Error::other)?;
    }
    tx.commit().map_err(io::Error::other)?;
    // 对话历史由 WorkflowEngine 在条目完整后实时追加写盘（见 DESIGN：
    // 流式输出合并成完整条目后立即追加写入磁盘），save 只持久化元数据。
    // 同理活动也由引擎实时追加，不回写快照。
    Ok(())
}

/// 惰性加载（元数据）：仅从 sqlite 读取会话骨架，不读取 transcript/activities
/// 两份 JSONL 文件体。调用方仅在渲染对话/活动视图（`load_payload`）时才按需补齐。
pub fn load_meta_window(data_dir: &Path, limit: usize) -> io::Result<(Vec<OrcSession>, bool)> {
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    let query_limit = limit.saturating_add(1) as i64;
    let mut stmt = conn
        .prepare(&meta_select(
            "ORDER BY last_active_at DESC, id DESC LIMIT ?1",
        ))
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([query_limit], read_meta_row)
        .map_err(io::Error::other)?;
    let mut sessions: Vec<OrcSession> = rows.map(meta_row_to_session).collect::<io::Result<_>>()?;
    let has_more = sessions.len() > limit;
    sessions.truncate(limit);
    let linked = load_linked_sessions(&conn).map_err(io::Error::other)?;
    attach_linked_sessions(&mut sessions, linked);
    Ok((sessions, has_more))
}

/// 全库工作流的关联普通会话 id 集合。工作流可能未加载进内存（分页窗口外），
/// 顶层普通会话过滤必须覆盖全库，否则窗口外工作流的关联会话会漏出。
pub fn load_all_linked_session_ids(data_dir: &Path) -> io::Result<HashSet<String>> {
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    let mut stmt = conn
        .prepare("SELECT session_id FROM workflow_linked_sessions")
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(io::Error::other)?;
    let mut ids = HashSet::new();
    for row in rows {
        ids.insert(row.map_err(io::Error::other)?);
    }
    Ok(ids)
}

pub fn load_all_meta(data_dir: &Path) -> io::Result<Vec<OrcSession>> {
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    let mut stmt = conn
        .prepare(&meta_select("ORDER BY last_active_at DESC, id DESC"))
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([], read_meta_row)
        .map_err(io::Error::other)?;
    let mut sessions: Vec<OrcSession> = rows.map(meta_row_to_session).collect::<io::Result<_>>()?;
    let linked = load_linked_sessions(&conn).map_err(io::Error::other)?;
    attach_linked_sessions(&mut sessions, linked);
    Ok(sessions)
}

/// 惰性加载（按需补齐）：读取指定会话的 transcript/activities payload。
/// 调用方可先在锁外读盘、再短暂持锁合并——避免持写锁做 IO 阻塞渲染与后台推进。
pub fn load_payload(data_dir: &Path, id: &str) -> io::Result<(Vec<OrcMsg>, Vec<Activity>)> {
    let transcript = transcript_from_history(&read_jsonl(&history_path(data_dir, id))?);
    let activities = read_jsonl(&activities_path(data_dir, id))?;
    Ok((transcript, activities))
}

pub fn remove(data_dir: &Path, id: &str) -> io::Result<()> {
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
        .map_err(io::Error::other)?;
    conn.execute(
        "DELETE FROM workflow_linked_sessions WHERE workflow_id = ?1",
        params![id],
    )
    .map_err(io::Error::other)?;
    for path in [history_path(data_dir, id), activities_path(data_dir, id)] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SessionState;

    fn temp() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("amux-wfstore-{}-{n}", std::process::id()))
    }

    /// 模拟引擎侧行为：用户输入与编排输出在条目完整后逐条追加写 history JSONL。
    fn append_history_transcript(dir: &Path, session_id: &str) {
        append_jsonl(
            &history_path(dir, session_id),
            &[
                HistoryItem::UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "开始".into(),
                    }],
                    timestamp: 1,
                },
                HistoryItem::AgentMessage {
                    content: vec![ContentBlock::Text {
                        text: "已转发".into(),
                    }],
                    timestamp: 2,
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn save_remove_matches_design_layout() {
        let dir = temp();
        let _ = std::fs::remove_dir_all(&dir);
        let session = OrcSession {
            id: "orc_1".into(),
            title: "计划A".into(),
            plan: "第一步：实现\n第二步：审查".into(),
            description: "做完再审查".into(),
            preamble: "计划".into(),
            state: SessionState::Idle,
            transcript: vec![
                OrcMsg::User {
                    text: "开始".into(),
                    timestamp: 1,
                },
                OrcMsg::Orc {
                    text: "已转发".into(),
                    timestamp: 2,
                },
            ],
            linked_sessions: vec![],
            activities: vec![Activity::Thinking {
                timestamp: 1,
                content: "想".into(),
            }],
            created_at: 10,
            updated_at: 20,
        };
        save(&dir, &session).unwrap(); // save 只写元数据
                                       // 对话历史由引擎在条目完整后实时追加写盘；这里模拟引擎追加。
        append_history_transcript(&dir, "orc_1");
        assert!(dir.join("session.sqlite").is_file());
        assert!(dir.join("sessions/orc_1_history.jsonl").is_file());
        // 活动由引擎实时追加写盘，save 不生成活动文件。
        assert!(!dir.join("sessions/orc_1_activities.jsonl").exists());

        remove(&dir, "orc_1").unwrap();
        assert!(load_all_meta(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linked_sessions_persist_in_dedicated_table() {
        let dir = temp();
        let _ = std::fs::remove_dir_all(&dir);
        let mut base = OrcSession {
            id: "orc_1".into(),
            title: String::new(),
            plan: String::new(),
            description: String::new(),
            preamble: String::new(),
            state: SessionState::Idle,
            transcript: Vec::new(),
            linked_sessions: vec![
                LinkedSession {
                    id: "s1".into(),
                    machine_name: "m1".into(),
                },
                LinkedSession {
                    id: "s1".into(),
                    machine_name: "m1".into(),
                },
            ],
            activities: Vec::new(),
            created_at: 1,
            updated_at: 1,
        };
        let mut second = base.clone();
        second.id = "orc_2".into();
        second.linked_sessions = vec![LinkedSession {
            id: "s2".into(),
            machine_name: "m1".into(),
        }];
        save(&dir, &base).unwrap();
        save(&dir, &second).unwrap();

        // 全库收集（供顶层普通会话过滤）：跨工作流去重
        let ids = load_all_linked_session_ids(&dir).unwrap();
        assert_eq!(
            ids,
            HashSet::from(["s1".to_string(), "s2".to_string()]),
            "重复挂载去重"
        );

        // 加载回读：机器名保留
        let meta = load_all_meta(&dir).unwrap();
        let orc_1 = meta.iter().find(|m| m.id == "orc_1").unwrap();
        assert_eq!(orc_1.linked_sessions.len(), 1, "重复挂载去重");
        assert_eq!(orc_1.linked_sessions[0].machine_name, "m1");

        // save 以内存为权威整组替换关联
        base.linked_sessions = vec![LinkedSession {
            id: "s3".into(),
            machine_name: "m2".into(),
        }];
        save(&dir, &base).unwrap();
        let ids = load_all_linked_session_ids(&dir).unwrap();
        assert_eq!(
            ids,
            HashSet::from(["s2".to_string(), "s3".to_string()]),
            "旧关联应被整组替换"
        );

        // 删除工作流级联删除其关联
        remove(&dir, "orc_2").unwrap();
        let ids = load_all_linked_session_ids(&dir).unwrap();
        assert_eq!(ids, HashSet::from(["s3".to_string()]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_window_limits_and_orders_sessions() {
        let dir = temp();
        let _ = std::fs::remove_dir_all(&dir);
        let oldest = OrcSession {
            id: "orc_old".into(),
            title: "旧工作流".into(),
            plan: String::new(),
            description: String::new(),
            preamble: String::new(),
            state: SessionState::Idle,
            transcript: Vec::new(),
            linked_sessions: Vec::new(),
            activities: Vec::new(),
            created_at: 1,
            updated_at: 10,
        };
        let mut middle = oldest.clone();
        middle.id = "orc_middle".into();
        middle.title = "中间工作流".into();
        middle.updated_at = 20;
        let mut newest = oldest.clone();
        newest.id = "orc_new".into();
        newest.title = "新工作流".into();
        newest.updated_at = 30;
        save(&dir, &oldest).unwrap();
        save(&dir, &middle).unwrap();
        save(&dir, &newest).unwrap();

        let (window, has_more) = load_meta_window(&dir, 2).unwrap();
        assert!(has_more);
        assert_eq!(
            window
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["orc_new", "orc_middle"]
        );
        let (window, has_more) = load_meta_window(&dir, 3).unwrap();
        assert!(!has_more);
        assert_eq!(window.len(), 3);
        assert_eq!(window[2].id, "orc_old");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_lazy_load_then_backfill_restores_payload() {
        let dir = temp();
        let _ = std::fs::remove_dir_all(&dir);
        let session = OrcSession {
            id: "orc_1".into(),
            title: "计划A".into(),
            plan: "第一步：实现\n第二步：审查".into(),
            description: "做完再审查".into(),
            preamble: "计划".into(),
            state: SessionState::Idle,
            transcript: vec![
                OrcMsg::User {
                    text: "开始".into(),
                    timestamp: 1,
                },
                OrcMsg::Orc {
                    text: "已转发".into(),
                    timestamp: 2,
                },
            ],
            linked_sessions: vec![],
            activities: vec![Activity::Thinking {
                timestamp: 1,
                content: "想".into(),
            }],
            created_at: 10,
            updated_at: 20,
        };
        save(&dir, &session).unwrap();
        // 对话历史由引擎在条目完整后实时追加写盘；这里模拟引擎追加。
        append_history_transcript(&dir, "orc_1");
        // 活动由引擎实时追加写盘；这里模拟已实时追加的活动，供 backfill 恢复。
        let act_path = activities_path(&dir, "orc_1");
        append_jsonl(
            &act_path,
            &[Activity::Thinking {
                timestamp: 1,
                content: "想".into(),
            }],
        )
        .unwrap();

        // 惰性元数据加载：只读 sqlite，不触碰 JSONL 文件体。
        let mut meta = load_all_meta(&dir).unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].id, "orc_1");
        assert_eq!(meta[0].title, "计划A");
        assert_eq!(meta[0].description, "做完再审查");
        // 执行计划列完整往返
        assert_eq!(meta[0].plan, "第一步：实现\n第二步：审查");
        assert_eq!(meta[0].preamble, "计划");
        assert_eq!(meta[0].state, SessionState::Idle);
        assert!(meta[0].transcript.is_empty());
        assert!(meta[0].activities.is_empty());

        // 按需补齐：打开会话时才把 transcript/activities 从 JSONL 恢复出来。
        let (transcript, activities) = load_payload(&dir, &meta[0].id).unwrap();
        meta[0].transcript = transcript;
        meta[0].activities = activities;
        assert_eq!(meta[0].transcript.len(), 2);
        assert!(matches!(
            &meta[0].transcript[0],
            OrcMsg::User { text, timestamp } if text == "开始" && *timestamp == 1
        ));
        assert!(matches!(
            &meta[0].transcript[1],
            OrcMsg::Orc { text, timestamp } if text == "已转发" && *timestamp == 2
        ));
        assert_eq!(meta[0].activities.len(), 1);
        assert_eq!(
            meta[0].activities[0],
            Activity::Thinking {
                timestamp: 1,
                content: "想".into()
            }
        );
        // 补齐只影响 payload，元数据字段保持不变。
        assert_eq!(meta[0].title, "计划A");
        assert_eq!(meta[0].preamble, "计划");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
