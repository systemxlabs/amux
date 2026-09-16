//! git 能力：diff 查询与改动撤销（git CLI）。
//!
//! `git diff` 的 unified diff 输出天然按 hunk 组织，可直接拆分为按块反向应用的
//! patch（`git apply --reverse`）；untracked 文件的 patch 由本模块按其内容合成。

use std::path::{Path, PathBuf};
use std::process::Command;

use amux_common::domain::{
    GitChangeStatus, GitDiffFile, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitDiffResult,
    OpResult,
};

use crate::fs::canonical_workspace_root;

#[derive(Default)]
pub struct GitRunner;

/// git CLI 输出错误。
#[derive(Debug)]
pub struct GitError {
    pub message: String,
    pub stderr: String,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GitError {}

fn run(cwd: &str, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output();
    match out {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(GitError {
            message: format!("git {} 失败", args.join(" ")),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Err(e) => Err(GitError {
            message: format!("git 执行失败: {e}"),
            stderr: String::new(),
        }),
    }
}

/// 构造失败的 `OpResult`（restore 的 CLI/文件操作错误路径共用）。
fn op_ok() -> OpResult {
    OpResult {
        ok: true,
        message: None,
    }
}

fn op_err(message: impl Into<String>) -> OpResult {
    OpResult {
        ok: false,
        message: Some(message.into()),
    }
}

/// 仓库根目录（worktree 根）；非仓库与 bare 仓库返回 None。
fn repo_root(cwd: &str) -> Option<PathBuf> {
    let out = run(cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    let root = out.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// 把相对 cwd 的路径换算为仓库根相对路径（cwd 通常即仓库根，映射为恒等）。
fn repo_relative_path(workdir: &Path, cwd: &str, p: &str) -> String {
    match Path::new(cwd).strip_prefix(workdir) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.join(p).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}

/// 把仓库根相对路径换算为相对 cwd 的路径——diff 结果的 path 与
/// `workspace.restore` 的 path 同基准（相对 cwd）。
fn cwd_relative_path(workdir: &Path, cwd: &str, repo_path: &str) -> String {
    match Path::new(cwd).strip_prefix(workdir) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            match Path::new(repo_path).strip_prefix(rel) {
                Ok(p) => p.to_string_lossy().into_owned(),
                // cwd 之外（如 ../x）的改动保持仓库根表示
                _ => repo_path.to_string(),
            }
        }
        _ => repo_path.to_string(),
    }
}

/// 读取工作区条目时不跟随符号链接。Git 将符号链接内容定义为其目标路径，
/// 而不是目标文件内容；跟随链接会把工作区外的文件泄漏到 diff 响应中。
fn worktree_bytes(path: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return std::fs::read_link(path)
            .ok()
            .map(|target| target.to_string_lossy().into_owned().into_bytes());
    }
    metadata
        .is_file()
        .then(|| std::fs::read(path).ok())
        .flatten()
}

/// 按行数（以 `\n` 计）与每行内容构造整文件新增 patch（untracked 文件用）。
fn whole_file_patch(path: &str, bytes: &[u8]) -> (String, Vec<GitDiffHunk>, u32) {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let n = lines.len() as u32;
    let header = format!("diff --git a/{path} b/{path}\n--- /dev/null\n+++ b/{path}\n");
    let hunk_hdr = format!("@@ -0,0 +1,{n} @@\n");
    let body: String = lines.iter().map(|l| format!("+{l}\n")).collect();
    let patch = format!("{header}{hunk_hdr}{body}");
    let hunks = vec![GitDiffHunk {
        header: hunk_hdr.trim_end().to_string(),
        patch: patch.clone(),
        lines: lines
            .iter()
            .map(|l| GitDiffLine {
                kind: GitDiffLineKind::Add,
                text: (*l).to_string(),
            })
            .collect(),
    }];
    (patch, hunks, n)
}

/// hunk 体行 → 行数组（前缀 ` `/`+`/`-`；`\` 换行标记行跳过）。
fn hunk_lines<'a>(body: impl Iterator<Item = &'a str>) -> Vec<GitDiffLine> {
    body
        .filter_map(|line| {
            let (prefix, text) = line.split_at(line.len().min(1));
            let kind = match prefix {
                " " => GitDiffLineKind::Context,
                "+" => GitDiffLineKind::Add,
                "-" => GitDiffLineKind::Remove,
                _ => return None,
            };
            Some(GitDiffLine {
                kind,
                text: text.to_string(),
            })
        })
        .collect()
}

/// 把 `git diff` 单文件输出拆分为按 hunk 可独立反向应用的 patch，并统计 +/- 行数。
/// 一个路径可能对应多个 `diff --git` 区段（文件↔符号链接的替换产生删除+新增两段），
/// 各 hunk 的 patch 取所属区段的 header。
fn parse_file_patch(raw: &str) -> (Vec<GitDiffHunk>, u32, u32) {
    // 区段边界（"diff --git " 行）与 hunk 边界（"@@ " 行）的字节偏移
    let mut boundaries = Vec::new();
    let mut off = 0;
    for line in raw.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            boundaries.push((off, false));
        } else if line.starts_with("@@ ") {
            boundaries.push((off, true));
        }
        off += line.len();
    }

    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    // 当前区段的 header 区间：首 hunk 出现时确定为 [区段起点, 首个 @@ 起点)
    let mut header: Option<(usize, usize)> = None;
    for (i, &(off, is_hunk)) in boundaries.iter().enumerate() {
        if !is_hunk {
            header = None;
            continue;
        }
        let end = boundaries
            .get(i + 1)
            .map(|(o, _)| *o)
            .unwrap_or(raw.len());
        let (hs, he) = match header {
            Some(range) => range,
            None => {
                let sec_off = boundaries[..i]
                    .iter()
                    .rev()
                    .find(|(_, hunk)| !*hunk)
                    .map(|(o, _)| *o)
                    .unwrap_or(0);
                let range = (sec_off, off);
                header = Some(range);
                range
            }
        };
        let body = &raw[off..end];
        let title = body.lines().next().unwrap_or_default().to_string();
        // skip(1)：@@ 头行之后，仅 +/- 行计入增删统计
        let lines = hunk_lines(body.lines().skip(1));
        for line in &lines {
            match line.kind {
                GitDiffLineKind::Add => additions += 1,
                GitDiffLineKind::Remove => deletions += 1,
                GitDiffLineKind::Context => {}
            }
        }
        hunks.push(GitDiffHunk {
            header: title,
            patch: format!("{}{}", &raw[hs..he], body),
            lines,
        });
    }
    (hunks, additions, deletions)
}

/// 文件改动状态：patch 中 `--- /dev/null` 为新增，`+++ /dev/null` 且文件已不存在为删除。
fn patch_status(raw: &str, on_disk: bool) -> GitChangeStatus {
    let has = |marker: &str| raw.lines().any(|line| line.starts_with(marker));
    if has("+++ /dev/null") && !on_disk {
        GitChangeStatus::Deleted
    } else if has("--- /dev/null") && !has("--- a/") {
        GitChangeStatus::Added
    } else {
        GitChangeStatus::Modified
    }
}

impl GitRunner {
    pub fn new() -> Self {
        GitRunner
    }

    /// 结构化 diff（git CLI）。
    /// cwd 非 git 仓库时返回 `not_repo` 标记。
    /// 重命名检测关闭：重命名显示为删除+新增（`git diff --no-renames` 语义）。
    pub fn diff(&self, cwd: &str, path: Option<&str>) -> GitDiffResult {
        let empty = || GitDiffResult {
            files: Vec::new(),
            not_repo: false,
        };
        let Some(root) = repo_root(cwd) else {
            return GitDiffResult {
                files: Vec::new(),
                not_repo: true,
            };
        };
        let root_str = root.to_string_lossy().into_owned();
        // 无提交（unborn HEAD）时 diff 无基准 → 空结果
        if run(&root_str, &["rev-parse", "--verify", "HEAD"]).is_err() {
            return empty();
        }
        // path 过滤：参数以 cwd 为基准，换算为仓库根相对路径
        let filter = path.map(|p| repo_relative_path(&root, cwd, p));
        let mut files = Vec::new();

        // tracked 变更：`git diff HEAD` 覆盖 staged + unstaged（含删除）
        let mut query = vec!["diff", "HEAD", "--no-renames", "-z", "--name-only"];
        if let Some(f) = &filter {
            query.extend(["--", f.as_str()]);
        }
        if let Ok(out) = run(&root_str, &query) {
            for name in out.split('\0').filter(|name| !name.is_empty()) {
                let Ok(patch) = run(&root_str, &["diff", "HEAD", "--no-renames", "--", name])
                else {
                    continue;
                };
                if patch.is_empty() {
                    continue;
                }
                let on_disk = std::fs::symlink_metadata(root.join(name)).is_ok();
                let (hunks, additions, deletions) = parse_file_patch(&patch);
                files.push(GitDiffFile {
                    path: cwd_relative_path(&root, cwd, name),
                    status: patch_status(&patch, on_disk),
                    additions,
                    deletions,
                    patch,
                    hunks,
                });
            }
        }

        // untracked：`git diff` 不覆盖，按其内容合成整文件新增 patch
        let mut query = vec!["ls-files", "--others", "--exclude-standard", "-z"];
        if let Some(f) = &filter {
            query.extend(["--", f.as_str()]);
        }
        if let Ok(out) = run(&root_str, &query) {
            for name in out.split('\0').filter(|name| !name.is_empty()) {
                let Some(bytes) = worktree_bytes(&root.join(name)) else {
                    continue;
                };
                let (patch, hunks, additions) = whole_file_patch(name, &bytes);
                files.push(GitDiffFile {
                    path: cwd_relative_path(&root, cwd, name),
                    status: GitChangeStatus::Added,
                    additions,
                    deletions: 0,
                    patch,
                    hunks,
                });
            }
        }

        files.sort_by(|a, b| a.path.cmp(&b.path));
        GitDiffResult {
            files,
            not_repo: false,
        }
    }

    /// 撤销工作区变更。
    /// - `patch`：单 hunk/单文件 patch 反向应用——`git apply --reverse`
    ///   （diff patch 的 a/ b/ 头为仓库根相对路径，故从仓库根执行 apply，cwd 为子目录时同样正确）
    /// - `path`：单文件——tracked 用 `git restore`；untracked 直接删除（从未提交，revert = 移除）
    /// - 都不给：全部变更——`git restore` 全部 tracked 变更 + `git clean` 全部 untracked
    pub fn restore(&self, cwd: &str, path: Option<&str>, patch: Option<&str>) -> OpResult {
        if let Some(p) = patch {
            // 唯一临时目录（并发 revert 不互相覆盖；uuid v4），apply 后立即清理
            let dir = std::env::temp_dir().join(format!("amux-revert-{}", uuid::Uuid::new_v4()));
            if std::fs::create_dir_all(&dir).is_err() {
                return op_err("创建临时目录失败");
            }
            let cleanup = || {
                let _ = std::fs::remove_dir_all(&dir);
            };
            let patch_file = dir.join("revert.patch");
            if std::fs::write(&patch_file, p).is_err() {
                cleanup();
                return op_err("写入 patch 失败");
            }
            // patch 路径以仓库根为基准：从仓库根应用（cwd 为其子目录时也正确）
            let apply_dir = repo_root(cwd).unwrap_or_else(|| PathBuf::from(cwd));
            let result = match run(
                apply_dir.to_string_lossy().as_ref(),
                &["apply", "--reverse", patch_file.to_string_lossy().as_ref()],
            ) {
                Ok(_) => op_ok(),
                Err(e) => op_err(e.stderr.trim()),
            };
            cleanup();
            return result;
        }
        if let Some(target) = path {
            let target = match validate_restore_path(cwd, target) {
                Ok(target) => target,
                Err(error) => return error,
            };
            // untracked：从未提交，revert = 删除工作区文件
            if self.is_untracked(cwd, &target) {
                return match std::fs::remove_file(std::path::Path::new(cwd).join(&target)) {
                    Ok(_) => op_ok(),
                    Err(e) => op_err(format!("删除 untracked 文件失败: {e}")),
                };
            }
            return match run(cwd, &["restore", "--staged", "--worktree", "--", &target]) {
                Ok(_) => op_ok(),
                Err(e) => op_err(e.stderr.trim()),
            };
        }
        if let Err(e) = run(cwd, &["restore", "--staged", "--worktree", "--", "."]) {
            return op_err(e.stderr.trim());
        }
        match run(cwd, &["clean", "-fd"]) {
            Ok(_) => op_ok(),
            Err(e) => OpResult {
                ok: false,
                message: Some(e.stderr.trim().to_string()),
            },
        }
    }

    /// 目标路径是否 untracked（`git status` 输出 `??` 前缀：既不在 HEAD 也不在索引中）。
    fn is_untracked(&self, cwd: &str, target: &str) -> bool {
        run(cwd, &["status", "--porcelain", "-z", "--", target])
            .map(|out| out.starts_with("??"))
            .unwrap_or(false)
    }

    /// 新建 worktree：目录约定 `<amux_home>/worktrees/<仓库目录名>-<随机串>/`。
    pub fn worktree_new(&self, repo: &str) -> Result<String, String> {
        let target = worktree_dir_for(repo, &amux_common::paths::worktrees_dir())?;
        self.add_worktree(repo, &target)?;
        Ok(target.to_string_lossy().replace('\\', "/"))
    }

    /// 按原路径重建 worktree（过期清理后按需重建；目录元数据仍保留在会话中）。
    pub fn worktree_resume(&self, repo: &str, path: &str) -> Result<String, String> {
        let target = PathBuf::from(path);
        self.rebuild_worktree(repo, &target)?;
        Ok(target.to_string_lossy().replace('\\', "/"))
    }

    /// 查询仓库的所有 worktree 路径。
    pub fn worktree_list(&self, repo: &str) -> Result<Vec<String>, String> {
        let out = run(repo, &["worktree", "list", "--porcelain"])
            .map_err(|e| format!("{}: {}", e.message, e.stderr.trim()))?;
        Ok(out
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(|path| path.to_string())
            .collect())
    }

    /// 删除 worktree（强制：会话删除级联清理不因未提交改动而失败）。
    pub fn worktree_remove(&self, repo: &str, path: &str) {
        self.remove_worktree(repo, Path::new(path));
    }

    /// 在 `repo_cwd` 仓库内创建指向 `target` 路径的 git worktree。分支名由 git
    /// 取目标目录 basename 自动生成。要求仓库已有提交（unborn HEAD 无法建 worktree）。
    fn add_worktree(&self, repo_cwd: &str, target: &Path) -> Result<(), String> {
        run(
            repo_cwd,
            &["worktree", "add", target.to_string_lossy().as_ref()],
        )
        .map(|_| ())
        .map_err(|e| format!("{}: {}", e.message, e.stderr.trim()))
    }

    /// 过期清理后按原路径重建 worktree。目录已删但主仓库残留的 worktree 管理
    /// 条目会令同路径 add 被拒（"丢失但已注册"），先 prune 再 add；分支名仍取
    /// 目录 basename，若该分支已存在则检出现有分支，延续会话原工作分支。
    fn rebuild_worktree(&self, repo_cwd: &str, target: &Path) -> Result<(), String> {
        let _ = run(repo_cwd, &["worktree", "prune"]);
        self.add_worktree(repo_cwd, target)
    }

    /// 移除 worktree（强制），并 prune 主仓库的残留管理信息。
    /// repo 已不存在时退化为直接删目录。
    fn remove_worktree(&self, repo_cwd: &str, target: &Path) {
        let target_str = target.to_string_lossy().into_owned();
        if repo_root(repo_cwd).is_some() {
            if let Err(e) = run(
                repo_cwd,
                &["worktree", "remove", "--force", target_str.as_str()],
            ) {
                log::warn!("git worktree remove 失败（回退直接删目录）: {e}");
            } else {
                let _ = run(repo_cwd, &["worktree", "prune"]);
                return;
            }
        }
        if let Err(e) = std::fs::remove_dir_all(target) {
            log::error!("删除 worktree 目录失败 {}: {e}", target.display());
        }
        let _ = run(repo_cwd, &["worktree", "prune"]);
    }
}

/// worktree 目标目录：`<root>/<仓库目录名>-<随机串>`。
fn worktree_dir_for(repo: &str, root: &Path) -> Result<PathBuf, String> {
    let workdir = repo_root(repo).ok_or_else(|| format!("不是 git 仓库: {repo}"))?;
    let name = workdir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    std::fs::create_dir_all(root).map_err(|e| format!("创建 worktree 根目录失败: {e}"))?;
    Ok(root.join(format!("{name}-{}", uuid::Uuid::new_v4())))
}

fn validate_restore_path(cwd: &str, target: &str) -> Result<String, OpResult> {
    let root = match canonical_workspace_root(cwd) {
        Ok(root) => root,
        Err(message) => return Err(op_err(message)),
    };
    let relative = Path::new(target);
    if target.is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(op_err("工作目录路径非法"));
    }

    // 逐个检查已存在的组件，而不是只 canonicalize 最终路径。最终目标可能是
    // 尚不存在的 untracked 文件；此时若父目录是指向工作区外的 symlink，
    // remove_file 会跟随它并误删工作区之外的文件。
    let mut current = root.clone();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(op_err(format!("工作目录路径不可访问: {error}"))),
        };
        if metadata.file_type().is_symlink() {
            return Err(op_err("工作目录路径包含符号链接"));
        }
        if !current.starts_with(&root) {
            return Err(op_err("工作目录路径超出工作目录范围"));
        }
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(cwd: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} 失败: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn unique_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }

    fn init_repo() -> std::path::PathBuf {
        let dir = unique_dir("amux-git");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main", "-q"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "init", "-q"]);
        dir
    }

    #[test]
    fn diff_lists_changes() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        git(&dir, &["add", "new.txt"]);
        let r = GitRunner::new();
        let st = r.diff(dir.to_str().unwrap(), None);
        assert!(!st.not_repo);
        assert!(st
            .files
            .iter()
            .any(|f| f.path == "a.txt" && matches!(f.status, GitChangeStatus::Modified)));
        assert!(st
            .files
            .iter()
            .any(|f| f.path == "new.txt" && matches!(f.status, GitChangeStatus::Added)));
    }

    #[test]
    fn diff_includes_untracked_files() {
        let dir = init_repo();
        std::fs::write(dir.join("untracked.txt"), "not staged\n").unwrap();
        let result = GitRunner::new().diff(dir.to_str().unwrap(), None);
        let file = result
            .files
            .iter()
            .find(|file| file.path == "untracked.txt")
            .expect("未跟踪文件应出现在 diff 中");
        assert!(matches!(file.status, GitChangeStatus::Added));
        assert!(file.patch.contains("not staged"));
    }

    #[cfg(unix)]
    #[test]
    fn diff_reads_symlink_targets_as_link_contents_without_following_them() {
        let dir = init_repo();
        let outside = unique_dir("amux-diff-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "must not leak").unwrap();

        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("untracked-link")).unwrap();
        let runner = GitRunner::new();
        let diff = runner.diff(dir.to_str().unwrap(), None);
        let untracked = diff
            .files
            .iter()
            .find(|file| file.path == "untracked-link")
            .expect("未跟踪符号链接应出现在 diff 中");
        assert!(untracked.patch.contains("secret.txt"));
        assert!(!untracked.patch.contains("must not leak"));

        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("a.txt")).unwrap();
        let diff = runner.diff(dir.to_str().unwrap(), Some("a.txt"));
        let replaced = diff
            .files
            .iter()
            .find(|file| file.path == "a.txt")
            .expect("被符号链接替换的 tracked 文件应出现在 diff 中");
        assert!(replaced.patch.contains("secret.txt"));
        assert!(!replaced.patch.contains("must not leak"));
    }

    #[test]
    fn non_repo_marks_not_repo() {
        let dir = unique_dir("amux-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let st = GitRunner::new().diff(dir.to_str().unwrap(), None).not_repo;
        assert!(st);
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_symlink_parent() {
        // 目标父目录是指向工作区外的 symlink 时，restore 必须拒绝，
        // 否则 remove_file 会跟随链接误删工作区之外的文件。
        let dir = unique_dir("amux-restore-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let outside = unique_dir("amux-restore-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("to-delete.txt"), "must remain").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

        let result =
            GitRunner::new().restore(dir.to_str().unwrap(), Some("link/to-delete.txt"), None);
        assert!(!result.ok, "符号链接父目录下的路径必须被拒绝");
        assert!(
            outside.join("to-delete.txt").exists(),
            "工作区外文件不得被删除"
        );
    }

    #[test]
    fn diff_returns_structured_files() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\nline3\n").unwrap();
        std::fs::write(dir.join("new.txt"), "hello\nworld\n").unwrap();
        git(&dir, &["add", "new.txt"]);
        let d = GitRunner::new().diff(dir.to_str().unwrap(), None);
        assert!(!d.not_repo);
        let a = d.files.iter().find(|f| f.path == "a.txt").expect("a.txt");
        assert_eq!(a.additions, 2);
        assert_eq!(a.deletions, 1);
        assert!(matches!(a.status, GitChangeStatus::Modified));
        assert!(a.patch.contains("diff --git a/a.txt b/a.txt"));
        assert_eq!(a.hunks.len(), 1);
        assert!(a.hunks[0].header.starts_with("@@ "));
        assert!(a.hunks[0].patch.contains("diff --git"));
        let n = d
            .files
            .iter()
            .find(|f| f.path == "new.txt")
            .expect("new.txt");
        assert!(matches!(n.status, GitChangeStatus::Added));
        assert_eq!(n.additions, 2);
    }

    #[test]
    fn revert_single_file_restores_workspace() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\n").unwrap();
        let r = GitRunner::new();
        let res = r.restore(dir.to_str().unwrap(), Some("a.txt"), None);
        assert!(res.ok, "revert 失败: {:?}", res.message);
        let content = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert_eq!(content, "line1\nline2\n", "工作区应恢复到 HEAD");
        let res = r.restore(dir.to_str().unwrap(), Some("a.txt"), None);
        assert!(res.ok, "幂等 revert 应成功: {:?}", res.message);
    }

    #[test]
    fn revert_single_hunk_via_patch() {
        let dir = unique_dir("amux-git-hunk");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main", "-q"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        let base: Vec<String> = (1..=20).map(|i| format!("line{i}")).collect();
        std::fs::write(dir.join("a.txt"), base.join("\n") + "\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "init", "-q"]);

        let mut lines = base.clone();
        lines[2] = "CHANGED1".into();
        lines[17] = "CHANGED2".into();
        std::fs::write(dir.join("a.txt"), lines.join("\n") + "\n").unwrap();
        let d = GitRunner::new().diff(dir.to_str().unwrap(), Some("a.txt"));
        assert_eq!(d.files[0].hunks.len(), 2, "两处改动应为两个 hunk");
        let hunk = &d.files[0].hunks[0];
        let res = GitRunner::new().restore(dir.to_str().unwrap(), None, Some(&hunk.patch));
        assert!(res.ok, "hunk revert 失败: {:?}", res.message);
        let content = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        let expected: Vec<String> = (1..=20)
            .map(|i| {
                if i == 18 {
                    "CHANGED2".into()
                } else {
                    format!("line{i}")
                }
            })
            .collect();
        assert_eq!(
            content,
            expected.join("\n") + "\n",
            "只应还原第一个 hunk（第 18 行仍为 CHANGED2）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_untracked_file_removes_it() {
        let dir = init_repo();
        std::fs::write(dir.join("scratch.txt"), "temp\n").unwrap();
        let res = GitRunner::new().restore(dir.to_str().unwrap(), Some("scratch.txt"), None);
        assert!(res.ok, "untracked revert 失败: {:?}", res.message);
        assert!(!dir.join("scratch.txt").exists(), "untracked 应被删除");
    }

    #[test]
    fn restore_rejects_paths_outside_workspace() {
        let dir = init_repo();
        let outside = dir.parent().unwrap().join("amux-restore-outside.txt");
        std::fs::write(&outside, "must remain").unwrap();

        let result = GitRunner::new().restore(
            dir.to_str().unwrap(),
            Some("../amux-restore-outside.txt"),
            None,
        );
        assert!(!result.ok);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "must remain");

        let _ = std::fs::remove_file(outside);
    }

    /// 目录约定：`<worktrees 根>/<仓库目录名>-<随机串>`，且每次不同。
    #[test]
    fn worktree_dir_follows_amux_convention() {
        let repo = init_repo();
        let root = unique_dir("amux-worktrees");
        let repo_name = repo.file_name().unwrap().to_string_lossy().into_owned();

        let target = worktree_dir_for(repo.to_str().unwrap(), &root).unwrap();
        assert_eq!(target.parent(), Some(root.as_path()));
        let name = target.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&format!("{repo_name}-")), "{name}");

        let other = worktree_dir_for(repo.to_str().unwrap(), &root).unwrap();
        assert_ne!(target, other, "同一仓库的两次创建不应撞目录");
    }

    #[test]
    fn worktree_can_be_created_listed_and_removed() {
        let repo = init_repo();
        let repo_str = repo.to_str().unwrap();
        let root = unique_dir("amux-worktree-roundtrip");
        let git = GitRunner::new();
        let target = worktree_dir_for(repo_str, &root).unwrap();
        let target_name = target.file_name().unwrap().to_string_lossy().into_owned();

        git.add_worktree(repo_str, &target).unwrap();
        assert!(target.join(".git").exists(), "worktree 应为有效检出");

        let listed = git.worktree_list(repo_str).unwrap();
        assert!(
            listed.iter().any(|path| path.ends_with(&target_name)),
            "worktree.list 应包含新建目录: {listed:?}"
        );

        git.worktree_remove(repo_str, target.to_str().unwrap());
        assert!(!target.exists(), "remove 后目录应消失");
        let listed = git.worktree_list(repo_str).unwrap();
        assert!(
            !listed.iter().any(|path| path.ends_with(&target_name)),
            "移除后不应仍在列表中: {listed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
