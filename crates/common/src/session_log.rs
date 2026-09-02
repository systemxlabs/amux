//! 会话对话历史与活动记录的 JSONL 存储布局。
//!
//! server（权威日志）与 desktop（工作流会话本地缓存）共用同一套文件布局：
//! `<data_dir>/sessions/<session_id>_history.jsonl` 与 `<id>_activities.jsonl`，
//! 每行一条 JSON。读取与路径收口在此，避免两处硬编码布局漂移。

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 会话数据目录：`<data_dir>/sessions/`。
pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// 会话对话历史文件：`<data_dir>/sessions/<session_id>_history.jsonl`。
pub fn history_path(data_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir(data_dir).join(format!("{session_id}_history.jsonl"))
}

/// 会话活动记录文件：`<data_dir>/sessions/<session_id>_activities.jsonl`。
pub fn activities_path(data_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir(data_dir).join(format!("{session_id}_activities.jsonl"))
}

/// 追加 JSONL 行（append-only）。空输入直接返回；缺失父目录会创建。
pub fn append_jsonl<T: serde::Serialize>(path: &Path, entries: &[T]) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for entry in entries {
        let line = serde_json::to_string(entry)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// 原子替换 JSONL 文件：先将完整内容写入同目录临时文件，再 rename 覆盖目标。
/// 临时文件名包含进程 ID 和单调计数器，避免并发写者复用同一路径。
pub fn write_jsonl_atomic<T: serde::Serialize>(path: &Path, entries: &[T]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("jsonl.tmp.{}.{}", std::process::id(), counter));
    {
        let mut f = std::fs::File::create(&tmp)?;
        for entry in entries {
            let line = serde_json::to_string(entry).map_err(io::Error::other)?;
            writeln!(f, "{line}")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// 读取全部 JSONL 行；文件缺失视为空，损坏行带行号报错。
pub fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Vec<T>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    content
        .lines()
        .enumerate()
        .map(|(line, value)| {
            serde_json::from_str(value).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}:{}: {error}", path.display(), line + 1),
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn path_layout_matches_documented_layout() {
        let dir = Path::new("/data");
        assert_eq!(
            history_path(dir, "s1"),
            PathBuf::from("/data/sessions/s1_history.jsonl")
        );
        assert_eq!(
            activities_path(dir, "s1"),
            PathBuf::from("/data/sessions/s1_activities.jsonl")
        );
    }

    #[test]
    fn append_then_read_roundtrip_and_missing_is_empty() {
        let dir = temp_dir();
        let path = history_path(dir.path(), "s1");
        assert!(read_jsonl::<u32>(&path).unwrap().is_empty());

        append_jsonl(&path, &[1, 2, 3]).unwrap();
        assert_eq!(read_jsonl::<u32>(&path).unwrap(), vec![1, 2, 3]);

        append_jsonl(&path, &[4]).unwrap();
        assert_eq!(read_jsonl::<u32>(&path).unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn atomic_write_replaces_existing_content_and_supports_empty_files() {
        let dir = temp_dir();
        let path = history_path(dir.path(), "s1");

        write_jsonl_atomic(&path, &[1, 2]).unwrap();
        assert_eq!(read_jsonl::<u32>(&path).unwrap(), vec![1, 2]);

        write_jsonl_atomic(&path, &[3]).unwrap();
        assert_eq!(read_jsonl::<u32>(&path).unwrap(), vec![3]);

        write_jsonl_atomic::<u32>(&path, &[]).unwrap();
        assert!(read_jsonl::<u32>(&path).unwrap().is_empty());
        assert!(path.is_file());
    }

    #[test]
    fn corrupted_line_reports_line_number() {
        let dir = temp_dir();
        let path = activities_path(dir.path(), "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"ok\":true}\nnot-json\n").unwrap();
        let err = read_jsonl::<serde_json::Value>(&path).unwrap_err();
        assert!(err.to_string().contains(":2:"), "应报告第 2 行：{err}");
    }
}
