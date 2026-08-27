//! 工作流会话存储：
//! - 元数据：`~/.amux/app/session.sqlite`
//! - 对话历史：`~/.amux/app/sessions/<session_id>_history.jsonl`
//! - 活动历史：`~/.amux/app/sessions/<session_id>_activities.jsonl`
//!
//! 历史/活动文件布局与读取复用 `amux-common::session_log`；写采用整文件原子替换。

use std::io::{self, Write};
use std::path::Path;

#[cfg(test)]
use protocol::Activity;
use protocol::{ContentBlock, HistoryItem};
use rusqlite::{params, Connection};

use crate::workflow::{ChildSession, OrcMsg, OrcSession};

fn history_path(data_dir: &Path, id: &str) -> std::path::PathBuf {
    amux_common::session_log::history_path(data_dir, id)
}

fn activities_path(data_dir: &Path, id: &str) -> std::path::PathBuf {
    amux_common::session_log::activities_path(data_dir, id)
}

fn sqlite_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("session.sqlite")
}

/// 原子写：先写同目录临时文件再 rename 覆盖。持久化中途崩溃不会留下截断文件
/// （截断 jsonl 曾被读取端静默当空处理，等于无告警丢全部历史）。
fn write_jsonl<T: serde::Serialize>(path: &Path, items: &[T]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        for item in items {
            let line = serde_json::to_string(item).map_err(io::Error::other)?;
            writeln!(f, "{line}")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Vec<T>> {
    amux_common::session_log::read_jsonl(path)
}

fn history_from_transcript(transcript: &[OrcMsg]) -> Vec<HistoryItem> {
    transcript
        .iter()
        .map(|m| match m {
            OrcMsg::User { text, timestamp } => HistoryItem::UserMessage {
                content: vec![ContentBlock::Text { text: text.clone() }],
                timestamp: *timestamp,
            },
            OrcMsg::Orc { text, timestamp } => HistoryItem::AgentMessage {
                content: vec![ContentBlock::Text { text: text.clone() }],
                timestamp: *timestamp,
            },
        })
        .collect()
}

fn transcript_from_history(items: &[HistoryItem]) -> Vec<OrcMsg> {
    items
        .iter()
        .map(|h| match h {
            HistoryItem::UserMessage { content, timestamp } => {
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                OrcMsg::User {
                    text,
                    timestamp: *timestamp,
                }
            }
            HistoryItem::AgentMessage { content, timestamp } => {
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                OrcMsg::Orc {
                    text,
                    timestamp: *timestamp,
                }
            }
        })
        .collect()
}

fn open_db(data_dir: &Path) -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let conn = Connection::open(sqlite_path(data_dir))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            state TEXT NOT NULL,
            last_active_at INTEGER NOT NULL,
            children TEXT NOT NULL,
            description TEXT NOT NULL,
            preamble TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
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

pub fn save(data_dir: &Path, session: &OrcSession) -> io::Result<()> {
    let children = serde_json::to_string(&session.children).map_err(io::Error::other)?;
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    conn.execute(
        "INSERT INTO sessions
            (id, title, state, last_active_at, children, description, preamble,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
            title=excluded.title, state=excluded.state,
            last_active_at=excluded.last_active_at, children=excluded.children,
            description=excluded.description, preamble=excluded.preamble,
            created_at=excluded.created_at, updated_at=excluded.updated_at",
        params![
            session.id,
            session.title,
            session.state.as_str(),
            session.updated_at as i64,
            children,
            session.description,
            session.preamble,
            session.created_at as i64,
            session.updated_at as i64,
        ],
    )
    .map_err(io::Error::other)?;
    write_jsonl(
        &history_path(data_dir, &session.id),
        &history_from_transcript(&session.transcript),
    )?;
    // 活动由 WorkflowEngine 实时逐条追加写盘，save 不再整文件覆盖，
    // 避免与实时追加竞争同一活动文件。
    Ok(())
}

/// 惰性加载（元数据）：仅从 sqlite 读取会话骨架，不读取 transcript/activities
/// 两份 JSONL 文件体。调用方仅在渲染对话/活动视图（`load_payload`）时才按需补齐。
pub fn load_all_meta(data_dir: &Path) -> io::Result<Vec<OrcSession>> {
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, state, last_active_at, children, description, preamble,
                created_at, updated_at
         FROM sessions ORDER BY last_active_at DESC",
        )
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? as u64,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)? as u64,
                row.get::<_, i64>(8)? as u64,
            ))
        })
        .map_err(io::Error::other)?;
    rows.map(|row| -> io::Result<OrcSession> {
        let (
            id,
            title,
            state,
            last_active_at,
            children,
            description,
            preamble,
            created_at,
            updated_at,
        ) = row.map_err(io::Error::other)?;
        let children: Vec<ChildSession> =
            serde_json::from_str(&children).map_err(io::Error::other)?;
        Ok(OrcSession {
            id,
            title,
            description,
            preamble,
            state: state_from(&state)?,
            transcript: Vec::new(),
            children,
            activities: Vec::new(),
            created_at,
            updated_at: last_active_at.max(updated_at),
        })
    })
    .collect()
}

/// 惰性加载（按需补齐）：把指定会话的 transcript/activities 从其对应 JSONL 读入，
/// 其余字段保持不动。仅在打开会话渲染对话/活动视图时调用。
pub fn load_payload(data_dir: &Path, session: &mut OrcSession) -> io::Result<()> {
    session.transcript =
        transcript_from_history(&read_jsonl(&history_path(data_dir, &session.id))?);
    session.activities = read_jsonl(&activities_path(data_dir, &session.id))?;
    Ok(())
}

pub fn remove(data_dir: &Path, id: &str) -> io::Result<()> {
    let conn = open_db(data_dir).map_err(io::Error::other)?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
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

    #[test]
    fn save_remove_matches_design_layout() {
        let dir = temp();
        let _ = std::fs::remove_dir_all(&dir);
        let session = OrcSession {
            id: "orc_1".into(),
            title: "计划A".into(),
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
            children: vec![],
            activities: vec![Activity::Thinking {
                timestamp: 1,
                content: "想".into(),
            }],
            created_at: 10,
            updated_at: 20,
        };
        save(&dir, &session).unwrap();
        assert!(dir.join("session.sqlite").is_file());
        assert!(dir.join("sessions/orc_1_history.jsonl").is_file());
        // 活动由引擎实时追加写盘，save 不再生成活动文件。
        assert!(!dir.join("sessions/orc_1_activities.jsonl").exists());

        remove(&dir, "orc_1").unwrap();
        assert!(load_all_meta(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_lazy_load_then_backfill_restores_payload() {
        let dir = temp();
        let _ = std::fs::remove_dir_all(&dir);
        let session = OrcSession {
            id: "orc_1".into(),
            title: "计划A".into(),
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
            children: vec![],
            activities: vec![Activity::Thinking {
                timestamp: 1,
                content: "想".into(),
            }],
            created_at: 10,
            updated_at: 20,
        };
        save(&dir, &session).unwrap();
        // 活动由引擎实时追加写盘；这里模拟已实时追加的活动，供 backfill 恢复。
        let act_path = amux_common::session_log::activities_path(&dir, "orc_1");
        amux_common::session_log::append_jsonl(
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
        assert_eq!(meta[0].preamble, "计划");
        assert_eq!(meta[0].state, SessionState::Idle);
        assert!(meta[0].transcript.is_empty());
        assert!(meta[0].activities.is_empty());

        // 按需补齐：打开会话时才把 transcript/activities 从 JSONL 恢复出来。
        load_payload(&dir, &mut meta[0]).unwrap();
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
