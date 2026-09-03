//! 工作目录浏览：`workspace.list` / `workspace.read`。
//!
//! 浏览本身只用 `std::fs`，不依赖 git；从 `GitRunner` 拆出的原因是它和 diff/
//! restore 并无共享实现，只为复用同一套「工作区根规范化 + 路径越界防护」。
//! 这里把浏览与安全根模型收拢在一起，git 能力留在 `git.rs`。

use std::path::{Path, PathBuf};

use protocol::{WorkspaceEntry, WorkspaceListResult, WorkspaceReadResult};

/// 工作目录浏览器（无状态）。
#[derive(Default)]
pub struct WorkspaceBrowser;

impl WorkspaceBrowser {
    pub fn new() -> Self {
        WorkspaceBrowser
    }

    /// 分页列出 cwd 下的目录项。所有路径都限制在 cwd 内，避免工作目录浏览
    /// 被用作任意文件系统读取入口。
    pub fn list_workspace(
        &self,
        cwd: &str,
        path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<WorkspaceListResult, String> {
        let root = canonical_workspace_root(cwd)?;
        let dir = resolve_workspace_path(&root, path.unwrap_or(""))?;
        if !dir.is_dir() {
            return Err(format!("工作目录不是文件夹: {}", dir.display()));
        }

        let mut entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("读取工作目录失败: {e}"))?
            .map(|entry| {
                let entry = entry.map_err(|e| format!("读取目录项失败: {e}"))?;
                let metadata = entry
                    .metadata()
                    .map_err(|e| format!("读取目录项元数据失败: {e}"))?;
                let entry_path = entry.path();
                let relative = entry_path
                    .strip_prefix(&root)
                    .map_err(|_| "目录项不在工作目录内".to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                Ok(WorkspaceEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    path: relative,
                    is_dir: metadata.is_dir(),
                    size: metadata.len(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        entries.sort_by(|a, b| {
            a.is_dir
                .cmp(&b.is_dir)
                .reverse()
                .then_with(|| a.name.cmp(&b.name))
        });

        let limit = limit.clamp(1, 500);
        let start = offset.min(entries.len());
        let end = (start + limit).min(entries.len());
        Ok(WorkspaceListResult {
            path: path.unwrap_or("").to_string(),
            entries: entries[start..end].to_vec(),
            has_more: end < entries.len(),
            next_offset: end,
        })
    }

    /// 以 UTF-8 文本行分页读取工作目录内的文件。
    pub fn read_workspace(
        &self,
        cwd: &str,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<WorkspaceReadResult, String> {
        let root = canonical_workspace_root(cwd)?;
        let file = resolve_workspace_path(&root, path)?;
        if !file.is_file() {
            return Err(format!("工作目录文件不存在: {path}"));
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
        Ok(WorkspaceReadResult {
            path: path.to_string(),
            content,
            has_more: end < lines.len(),
            next_offset: end,
        })
    }
}

/// 规范化工作区根：目录浏览/文件读取都以它为边界，防止越界访问。
pub(crate) fn canonical_workspace_root(cwd: &str) -> Result<PathBuf, String> {
    let root = Path::new(cwd)
        .canonicalize()
        .map_err(|e| format!("工作目录不可访问: {e}"))?;
    if !root.is_dir() {
        return Err(format!("工作目录不是文件夹: {}", root.display()));
    }
    Ok(root)
}

/// 把相对工作区根解析为可读路径：拒绝绝对路径、父目录回溯与符号链接逃逸。
fn resolve_workspace_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("工作目录路径非法".to_string());
    }
    let path = root.join(relative_path);
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("工作目录路径不可访问: {e}"))?;
    if !canonical.starts_with(root) {
        return Err("工作目录路径超出工作目录范围".to_string());
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn list_sorts_directories_and_paginates() {
        let dir = unique_dir("amux-workspace-list");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("z.txt"), "z").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let result = WorkspaceBrowser::new()
            .list_workspace(dir.to_str().unwrap(), None, 2, 0)
            .unwrap();
        assert_eq!(result.path, "");
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].name, "src");
        assert_eq!(result.entries[1].name, "a.txt");
        assert!(result.has_more);
        assert_eq!(result.next_offset, 2);

        let next = WorkspaceBrowser::new()
            .list_workspace(dir.to_str().unwrap(), None, 2, result.next_offset)
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

    #[test]
    fn read_returns_line_pages() {
        let dir = unique_dir("amux-workspace-read");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        let browser = WorkspaceBrowser::new();

        let first = browser
            .read_workspace(dir.to_str().unwrap(), "notes.txt", 0, 2)
            .unwrap();
        assert_eq!(first.content, "one\ntwo\n");
        assert!(first.has_more);
        assert_eq!(first.next_offset, 2);

        let second = browser
            .read_workspace(dir.to_str().unwrap(), "notes.txt", first.next_offset, 2)
            .unwrap();
        assert_eq!(second.content, "three\n");
        assert!(!second.has_more);
    }

    #[test]
    fn paths_cannot_escape_root() {
        let dir = unique_dir("amux-workspace-safe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inside.txt"), "inside").unwrap();
        let browser = WorkspaceBrowser::new();
        let cwd = dir.to_str().unwrap();

        assert!(browser.list_workspace(cwd, Some("../"), 10, 0).is_err());
        assert!(browser.read_workspace(cwd, "/etc/passwd", 0, 10).is_err());

        #[cfg(unix)]
        {
            let outside = unique_dir("amux-workspace-outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.txt"), "secret").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            assert!(browser
                .read_workspace(cwd, "link/secret.txt", 0, 10)
                .is_err());
        }
    }
}
