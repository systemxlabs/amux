//! 会话对话历史与活动记录的 JSONL 存储布局。
//!
//! server（权威日志）与 desktop（工作流会话本地缓存）共用同一套文件布局：
//! `<data_dir>/sessions/<session_id>_history.jsonl` 与 `<id>_activities.jsonl`，
//! 每行一条 JSON。读取与路径收口在此，避免两处硬编码布局漂移。

use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// 会话数据目录：`<data_dir>/sessions/`。
fn sessions_dir(data_dir: &Path) -> PathBuf {
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

/// 读取 JSONL 的尾部窗口；`before` 是独占条目下标，与协议分页游标一致。
/// 先按字节统计行边界，再只反序列化请求窗口；避免长会话每次分页都解析全文件。
pub fn read_jsonl_page<T: serde::de::DeserializeOwned>(
    path: &Path,
    limit: usize,
    before: Option<u64>,
) -> io::Result<(Vec<T>, bool, Option<u64>)> {
    let limit = limit.max(1);
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false, None));
        }
        Err(error) => return Err(error),
    };
    let file_len = file.metadata()?.len() as usize;

    // JSONL 只追加；一次字节扫描即可得到总条数和 before 对应的字节边界，
    // 不需要把所有行反序列化成协议对象。
    let mut total = 0usize;
    let mut boundary = None;
    if file_len > 0 {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut offset = 0usize;
        let mut last_byte = 0u8;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            for (i, byte) in buffer[..n].iter().enumerate() {
                if *byte != b'\n' {
                    continue;
                }
                total += 1;
                if let Some(index) = before {
                    if total as u64 == index {
                        boundary = Some(offset + i + 1);
                    }
                }
            }
            last_byte = buffer[n - 1];
            offset += n;
        }
        if last_byte != b'\n' {
            total += 1;
            if before == Some(total as u64) {
                boundary = Some(file_len);
            }
        }
    }

    let end_index = before
        .map(|index| (index as usize).min(total))
        .unwrap_or(total);
    let end_pos = boundary.unwrap_or(file_len);
    let start_index = end_index.saturating_sub(limit);
    let has_more = start_index > 0;

    // 从尾部向前定位换行符；正常窗口只读取并反序列化 limit 行。
    let mut items = Vec::with_capacity(end_index - start_index);
    let mut cursor = end_pos;
    while cursor > 0 && items.len() < end_index - start_index {
        let Some((line_start, line_end)) = previous_line_range(&mut file, cursor, file_len)? else {
            break;
        };
        let line_no = end_index - items.len() - 1;
        let mut bytes = vec![0u8; line_end - line_start];
        file.seek(std::io::SeekFrom::Start(line_start as u64))?;
        file.read_exact(&mut bytes)?;
        let item = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}:{}: {error}", path.display(), line_no + 1),
            )
        })?;
        items.push(item);
        cursor = line_start;
    }
    items.reverse();
    Ok((items, has_more, has_more.then_some(start_index as u64)))
}

/// 在 `[0, cursor)` 中定位上一行；返回的字节区间不含换行符。
/// `cursor` 通常是文件长度或某行换行符后的位置。
fn previous_line_range(
    file: &mut std::fs::File,
    cursor: usize,
    file_len: usize,
) -> io::Result<Option<(usize, usize)>> {
    if cursor == 0 || file_len == 0 {
        return Ok(None);
    }
    let content_end = if read_byte_at(file, cursor - 1)? == Some(b'\n') {
        cursor - 1
    } else {
        cursor
    };

    const CHUNK: usize = 64 * 1024;
    let mut scan_end = content_end;
    while scan_end > 0 {
        let chunk_start = scan_end.saturating_sub(CHUNK);
        let len = scan_end - chunk_start;
        let mut buffer = vec![0u8; len];
        file.seek(std::io::SeekFrom::Start(chunk_start as u64))?;
        file.read_exact(&mut buffer)?;
        for i in (0..len).rev() {
            if buffer[i] == b'\n' {
                return Ok(Some((chunk_start + i + 1, content_end)));
            }
        }
        scan_end = chunk_start;
    }
    Ok(Some((0, content_end)))
}

fn read_byte_at(file: &mut std::fs::File, offset: usize) -> io::Result<Option<u8>> {
    if offset >= file.metadata()?.len() as usize {
        return Ok(None);
    }
    file.seek(std::io::SeekFrom::Start(offset as u64))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(Some(byte[0]))
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
    fn jsonl_page_reads_tail_and_earlier_windows() {
        let dir = temp_dir();
        let path = history_path(dir.path(), "s1");
        append_jsonl(&path, &(0..1000).collect::<Vec<_>>()).unwrap();

        let (items, has_more, next_before) = read_jsonl_page::<u32>(&path, 200, None).unwrap();
        assert_eq!(items, (800..1000).collect::<Vec<_>>());
        assert!(has_more);
        assert_eq!(next_before, Some(800));

        let (items, has_more, next_before) =
            read_jsonl_page::<u32>(&path, 200, next_before).unwrap();
        assert_eq!(items, (600..800).collect::<Vec<_>>());
        assert!(has_more);
        assert_eq!(next_before, Some(600));

        let (items, has_more, next_before) = read_jsonl_page::<u32>(&path, 200, Some(1)).unwrap();
        assert_eq!(items, vec![0]);
        assert!(!has_more);
        assert_eq!(next_before, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonl_page_missing_is_empty_and_reports_window_line_number() {
        let dir = temp_dir();
        let missing = history_path(dir.path(), "missing");
        let (items, has_more, next_before) = read_jsonl_page::<u32>(&missing, 10, None).unwrap();
        assert!(items.is_empty());
        assert!(!has_more);
        assert_eq!(next_before, None);

        let path = history_path(dir.path(), "bad");
        append_jsonl(&path, &[0]).unwrap();
        std::fs::write(&path, "0\nbad\n").unwrap();
        let error = read_jsonl_page::<u32>(&path, 10, None).unwrap_err();
        assert!(error.to_string().contains(":2:"), "应报告第 2 行：{error}");
        let _ = std::fs::remove_dir_all(&dir);
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
