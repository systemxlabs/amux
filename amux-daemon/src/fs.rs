//! 文件系统浏览：`fs.list` / `fs.read`。
//!
//! 浏览本身只用 `std::fs`，不依赖 git，也不要求路径在会话工作目录内——
//! 调用方直接给绝对路径（文档约定 `fs.*` 范围是「指定路径」）。

use std::path::{Path, PathBuf};

use amux_common::domain::{FsEntry, FsListResult, FsReadResult};

/// 文件浏览器（无状态）。
#[derive(Default)]
pub struct FsBrowser;

impl FsBrowser {
    pub fn new() -> Self {
        FsBrowser
    }

    /// 分页列出指定绝对路径目录下的条目，条目 path 为绝对路径。
    pub fn list(
        &self,
        path: Option<&str>,
        limit: usize,
        offset: usize,
        dirs_only: bool,
    ) -> Result<FsListResult, String> {
        let dir = canonical_dir(path.unwrap_or(""))?;

        let mut entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("读取目录失败: {e}"))?
            .map(|entry| {
                let entry = entry.map_err(|e| format!("读取目录项失败: {e}"))?;
                let metadata = entry
                    .metadata()
                    .map_err(|e| format!("读取目录项元数据失败: {e}"))?;
                Ok(FsEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    path: entry.path().to_string_lossy().replace('\\', "/"),
                    is_dir: metadata.is_dir(),
                    size: metadata.len(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        // 只列目录时在分页前过滤：页大小只计目录，避免被同目录下的大量文件挤掉
        if dirs_only {
            entries.retain(|entry| entry.is_dir);
        }
        entries.sort_by(|a, b| {
            a.is_dir
                .cmp(&b.is_dir)
                .reverse()
                .then_with(|| a.name.cmp(&b.name))
        });

        let limit = limit.clamp(1, 500);
        let start = offset.min(entries.len());
        let end = (start + limit).min(entries.len());
        Ok(FsListResult {
            path: path.unwrap_or("").to_string(),
            entries: entries[start..end].to_vec(),
            has_more: end < entries.len(),
            next_offset: end,
        })
    }

    /// 以 UTF-8 文本行分页读取指定绝对路径文件。
    pub fn read(&self, path: &str, offset: usize, limit: usize) -> Result<FsReadResult, String> {
        let file = canonical_path(path)?;
        if !file.is_file() {
            return Err(format!("文件不存在: {path}"));
        }
        let bytes = std::fs::read(&file).map_err(|e| format!("读取文件失败: {e}"))?;
        let text = std::str::from_utf8(&bytes).map_err(|_| "文件不是 UTF-8 文本".to_string())?;
        let lines: Vec<&str> = text.lines().collect();
        let limit = limit.clamp(1, 1_000);
        let start = offset.min(lines.len());
        let end = (start + limit).min(lines.len());
        let mut content = lines[start..end].join("\n");
        if end > start && (end < lines.len() || text.ends_with('\n')) {
            content.push('\n');
        }
        Ok(FsReadResult {
            path: path.to_string(),
            content,
            has_more: end < lines.len(),
            next_offset: end,
        })
    }
}

/// 规范化目录路径：canonicalize 以稳定路径表示（解析符号链接、消除 ..）。
pub(crate) fn canonical_dir(path: &str) -> Result<PathBuf, String> {
    let dir = canonical_path(path)?;
    if !dir.is_dir() {
        return Err(format!("不是文件夹: {}", dir.display()));
    }
    Ok(dir)
}

/// 规范化任意路径；空串按当前目录处理。
pub(crate) fn canonical_path(path: &str) -> Result<PathBuf, String> {
    let path = if path.is_empty() {
        Path::new(".")
    } else {
        Path::new(path)
    };
    path.canonicalize()
        .map_err(|e| format!("路径不可访问: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn list_sorts_directories_and_paginates() {
        let dir = unique_dir("amux-fs-list");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("z.txt"), "z").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let result = FsBrowser::new()
            .list(Some(dir.to_str().unwrap()), 2, 0, false)
            .unwrap();
        assert_eq!(result.path, dir.to_str().unwrap());
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].name, "src");
        assert_eq!(result.entries[1].name, "a.txt");
        // 条目 path 为绝对路径，客户端直接用于加载子目录/读文件
        assert_eq!(result.entries[0].path, dir.join("src").to_str().unwrap());
        assert!(result.has_more);
        assert_eq!(result.next_offset, 2);

        let next = FsBrowser::new()
            .list(Some(dir.to_str().unwrap()), 2, result.next_offset, false)
            .unwrap();
        assert_eq!(
            next.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["z.txt"]
        );
        assert!(!next.has_more);
    }

    /// 只列目录时在分页前过滤：页大小只计目录，不被同目录下的大量文件挤掉
    /// （docs/DESIGN.md「新建会话视图」：拉取全部目录项（不包括文件））。
    #[test]
    fn list_dirs_only_filters_before_paging() {
        let dir = unique_dir("amux-fs-dirs-only");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        for name in ["f1.txt", "f2.txt", "f3.txt"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }

        let first = FsBrowser::new()
            .list(Some(dir.to_str().unwrap()), 1, 0, true)
            .unwrap();
        assert_eq!(
            first
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
        assert!(first.entries[0].is_dir);
        assert!(first.has_more, "还有目录 b 未返回");

        let second = FsBrowser::new()
            .list(Some(dir.to_str().unwrap()), 1, first.next_offset, true)
            .unwrap();
        assert_eq!(
            second
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert!(!second.has_more);
    }

    #[test]
    fn list_accepts_any_absolute_path() {
        let dir = unique_dir("amux-fs-anywhere");
        std::fs::create_dir_all(&dir).unwrap();
        let result = FsBrowser::new()
            .list(Some(dir.to_str().unwrap()), 10, 0, false)
            .unwrap();
        assert!(result.entries.is_empty());
        // 任意绝对路径（如 /etc）也可列目录
        #[cfg(unix)]
        {
            let etc = FsBrowser::new().list(Some("/etc"), 500, 0, false).unwrap();
            assert!(etc.entries.iter().any(|e| e.name == "passwd"));
        }
    }

    #[test]
    fn read_returns_line_pages() {
        let dir = unique_dir("amux-fs-read");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let browser = FsBrowser::new();

        let first = browser.read(file.to_str().unwrap(), 0, 2).unwrap();
        assert_eq!(first.content, "one\ntwo\n");
        assert!(first.has_more);
        assert_eq!(first.next_offset, 2);

        let second = browser
            .read(file.to_str().unwrap(), first.next_offset, 2)
            .unwrap();
        assert_eq!(second.content, "three\n");
        assert!(!second.has_more);
    }
}
