//! git 能力：
//! - diff 查询、untracked 判定：gitoxide（gix）结构化实现，不依赖 git 二进制、
//!   无本地化输出解析
//! - gitoxide 无等价能力处保留 git CLI：worktree 新增/删除/重建（gix-worktree
//!   仅有列出能力，无增删管理）
//!
//! 依赖上用 `gix` 伞 crate（DESIGN「Daemon 技术栈 - Git：gix」，只开 status / sha1 两个
//! 特性）：这里需要的是「仓库访问 + status（含 dirwalk、忽略规则、attributes、过滤）」
//! 这一整套跨子系统能力，正是伞 crate 的 status 特性所封装的部分；改用细粒度子 crate 得
//! 自己复刻那几百行装配，收益仅是去掉伞 crate 作为非可选依赖带进来的 gix-protocol /
//! gix-revision 等（本项目用不到），代价与收益不成比例，故保留伞 crate。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::diff::blob::pipeline::{Mode, WorktreeRoots};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{ResourceKind, UnifiedDiff};

use amux_common::domain::{
    GitChangeStatus, GitDiffFile, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitDiffResult,
};

#[derive(Default)]
pub struct GitRunner;

/// git CLI 输出错误（`run` 执行失败时携带 stderr）。
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

/// 把仓库根相对路径换算为相对 cwd 的路径（diff 结果的 path 以 cwd 为基准）。
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

/// 单文件 unified diff 汇总：按 hunk 拆分的改动内容。
struct FilePatch {
    hunks: Vec<GitDiffHunk>,
    additions: u32,
    deletions: u32,
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

fn worktree_entry_kind(path: &Path) -> Option<gix::object::tree::EntryKind> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        Some(gix::object::tree::EntryKind::Link)
    } else if metadata.is_file() {
        Some(gix::object::tree::EntryKind::Blob)
    } else {
        None
    }
}

/// 统一 diff 渲染收集器：UnifiedDiff 逐个 hunk 回调，收集行级数据。
type HunkLines = Vec<(DiffLineKind, Vec<u8>)>;

#[derive(Default)]
struct HunkCollector {
    hunks: Vec<(HunkHeader, HunkLines)>,
}

impl ConsumeHunk for HunkCollector {
    type Out = Vec<(HunkHeader, HunkLines)>;

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        self.hunks.push((
            header,
            lines.iter().map(|(k, l)| (*k, l.to_vec())).collect(),
        ));
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.hunks
    }
}

/// git 风格的 hunk 头：`@@ -a,b +c,d @@`，行数为 1 时省略 `,1`（与 git 输出一致）。
fn hunk_header_text(h: &HunkHeader) -> String {
    let before = if h.before_hunk_len == 1 {
        format!("{}", h.before_hunk_start)
    } else {
        format!("{},{}", h.before_hunk_start, h.before_hunk_len)
    };
    let after = if h.after_hunk_len == 1 {
        format!("{}", h.after_hunk_start)
    } else {
        format!("{},{}", h.after_hunk_start, h.after_hunk_len)
    };
    format!("@@ -{before} +{after} @@")
}

fn diff_line_kind(kind: DiffLineKind) -> GitDiffLineKind {
    match kind {
        DiffLineKind::Context => GitDiffLineKind::Context,
        DiffLineKind::Add => GitDiffLineKind::Add,
        DiffLineKind::Remove => GitDiffLineKind::Remove,
    }
}

/// 整文件新增/删除：整份文件内容为一个 hunk（空的一侧没有 hunk）。
fn whole_file_patch(bytes: &[u8], added: bool) -> FilePatch {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let n = lines.len() as u32;
    let hunk_hdr = if added {
        format!("@@ -0,0 +1,{n} @@\n")
    } else {
        format!("@@ -1,{n} +0,0 @@\n")
    };
    let kind = if added {
        GitDiffLineKind::Add
    } else {
        GitDiffLineKind::Remove
    };
    let lines: Vec<GitDiffLine> = lines
        .into_iter()
        .map(|line| GitDiffLine {
            kind,
            text: line.to_string(),
        })
        .collect();
    let hunks = vec![GitDiffHunk {
        header: hunk_hdr.trim_end().to_string(),
        lines,
    }];
    FilePatch {
        hunks,
        additions: if added { n } else { 0 },
        deletions: if added { 0 } else { n },
    }
}

/// 修改文件：HEAD blob vs 工作区文件，经 gix blob diff + 统一 diff 渲染得到 hunk 结构。
fn modified_patch(
    repo: &gix::Repository,
    cache: &mut gix::diff::blob::Platform,
    old_id: gix::hash::ObjectId,
    old_mode: gix::object::tree::EntryKind,
    new_mode: gix::object::tree::EntryKind,
    path: &BStr,
) -> Option<FilePatch> {
    let new_id = gix::hash::ObjectId::null(repo.object_hash());
    cache
        .set_resource(
            old_id,
            old_mode,
            path,
            ResourceKind::OldOrSource,
            &repo.objects,
        )
        .ok()?;
    cache
        .set_resource(
            new_id,
            new_mode,
            path,
            ResourceKind::NewOrDestination,
            &repo.objects,
        )
        .ok()?;
    let prep = cache.prepare_diff().ok()?;
    let hunks_data = match prep.operation {
        Operation::InternalDiff { algorithm } => {
            let input = prep.interned_input();
            let diff = gix::diff::blob::diff_with_slider_heuristics(algorithm, &input);
            let ud = UnifiedDiff::new(
                &diff,
                &input,
                HunkCollector::default(),
                ContextSize::symmetrical(3),
            );
            ud.consume().ok()?
        }
        // 二进制等不可行内 diff 的资源：无 hunk（与 `git diff` 无内容时的表现一致）。
        Operation::SourceOrDestinationIsBinary => Vec::new(),
        Operation::ExternalCommand { .. } => unreachable!("内部 diff 选项已强制，不应走外部命令"),
    };
    let mut hunks = Vec::new();
    let mut additions = 0u32;
    let mut deletions = 0u32;
    for (h, lines) in hunks_data {
        additions += lines
            .iter()
            .filter(|(k, _)| *k == DiffLineKind::Add)
            .count() as u32;
        deletions += lines
            .iter()
            .filter(|(k, _)| *k == DiffLineKind::Remove)
            .count() as u32;
        let header = hunk_header_text(&h);
        let lines: Vec<GitDiffLine> = lines
            .iter()
            .map(|(kind, content)| GitDiffLine {
                kind: diff_line_kind(*kind),
                text: String::from_utf8_lossy(content).into_owned(),
            })
            .collect();
        hunks.push(GitDiffHunk { header, lines });
    }
    Some(FilePatch {
        hunks,
        additions,
        deletions,
    })
}

impl GitRunner {
    pub fn new() -> Self {
        GitRunner
    }

    /// 结构化 diff：gitoxide 实现。
    /// cwd 非 git 仓库时返回 `not_repo` 标记。
    pub fn diff(&self, cwd: &str) -> GitDiffResult {
        let empty = || GitDiffResult {
            files: Vec::new(),
            not_repo: false,
        };
        let repo = match gix::discover(cwd) {
            Ok(r) => r,
            // 非仓库（向上查找无 .git）或仓库不可用时，按 `git.diff` 协议返回 not_repo。
            Err(_) => {
                return GitDiffResult {
                    files: Vec::new(),
                    not_repo: true,
                };
            }
        };
        // bare 仓库没有工作区，diff 无意义（`git diff` 同场景报错）
        let Some(workdir) = repo.workdir() else {
            return GitDiffResult {
                files: Vec::new(),
                not_repo: true,
            };
        };
        // 无提交（unborn HEAD）时 `git diff HEAD` 失败 → 空结果
        let Ok(head_tree) = repo.head_tree() else {
            return empty();
        };

        // 收集变更路径：TreeIndex（HEAD vs index，staged）+ IndexWorktree（index vs 工作区，unstaged）。
        // 重命名检测关闭：重命名显示为删除+新增（`git diff --no-renames` 语义）。
        let mut paths = BTreeSet::<BString>::new();
        let status = match repo.status(gix::progress::Discard).map(|s| {
            s.untracked_files(gix::status::UntrackedFiles::Files)
                .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
                .index_worktree_rewrites(None)
        }) {
            Ok(s) => s,
            Err(_) => return empty(),
        };
        let iter = match status.into_iter(Vec::<BString>::new()) {
            Ok(i) => i,
            Err(_) => return empty(),
        };
        for item in iter {
            match item {
                Ok(gix::status::Item::TreeIndex(change)) => match change {
                    gix::diff::index::ChangeRef::Addition { location, .. }
                    | gix::diff::index::ChangeRef::Deletion { location, .. }
                    | gix::diff::index::ChangeRef::Modification { location, .. } => {
                        paths.insert(location.into_owned());
                    }
                    gix::diff::index::ChangeRef::Rewrite { .. } => {}
                },
                Ok(gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::Modification { rela_path, .. },
                )) => {
                    paths.insert(rela_path);
                }
                Ok(gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::DirectoryContents { entry, .. },
                )) => {
                    if matches!(
                        entry.disk_kind,
                        Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink)
                    ) {
                        paths.insert(entry.rela_path);
                    }
                }
                Ok(gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::Rewrite { dirwalk_entry, .. },
                )) => {
                    if matches!(
                        dirwalk_entry.disk_kind,
                        Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink)
                    ) {
                        paths.insert(dirwalk_entry.rela_path);
                    }
                }
                Err(_) => return empty(),
            }
        }

        let mut cache = match repo.diff_resource_cache(
            Mode::ToGit,
            WorktreeRoots {
                old_root: None,
                new_root: Some(workdir.to_path_buf()),
            },
        ) {
            Ok(c) => c,
            Err(_) => return empty(),
        };

        let mut files = Vec::new();
        for p in &paths {
            let p_str = p.to_str_lossy().into_owned();
            let old = head_tree
                .lookup_entry_by_path(gix::path::from_bstr(p))
                .ok()
                .flatten()
                .map(|e| (e.object_id(), e.mode()));
            let abs = workdir.join(PathBuf::from(p_str.clone()));
            let new_mode = worktree_entry_kind(&abs);
            let fp = match (&old, new_mode) {
                (Some((old_id, old_mode)), Some(new_mode)) => {
                    let fp = modified_patch(
                        &repo,
                        &mut cache,
                        *old_id,
                        old_mode.kind(),
                        new_mode,
                        p.as_ref(),
                    );
                    // blob diff 资源缓存只增不减，逐文件释放以免大 diff 时内存无界增长
                    cache.clear_resource_cache_keep_allocation();
                    match fp {
                        Some(fp) => fp,
                        None => continue,
                    }
                }
                (None, Some(_)) => {
                    let Some(bytes) = worktree_bytes(&abs) else {
                        continue;
                    };
                    whole_file_patch(&bytes, true)
                }
                (Some((old_id, _)), None) => {
                    let bytes = repo
                        .find_blob(*old_id)
                        .map(|b| b.data.to_vec())
                        .unwrap_or_default();
                    whole_file_patch(&bytes, false)
                }
                (None, None) => continue,
            };
            let status = match (&old, new_mode) {
                (None, Some(_)) => GitChangeStatus::Added,
                (Some(_), None) => GitChangeStatus::Deleted,
                _ => GitChangeStatus::Modified,
            };
            files.push(GitDiffFile {
                path: cwd_relative_path(workdir, cwd, &p_str),
                status,
                additions: fp.additions,
                deletions: fp.deletions,
                hunks: fp.hunks,
            });
        }
        GitDiffResult {
            files,
            not_repo: false,
        }
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

    /// 查询仓库的所有 worktree 路径（主工作树 + linked worktrees）。
    /// gix-worktree 可枚举工作树；若枚举失败则回退到 `git worktree list`。
    pub fn worktree_list(&self, repo: &str) -> Result<Vec<String>, String> {
        match self.worktree_list_gix(repo) {
            Ok(paths) => Ok(paths),
            Err(gix_err) => {
                let out = run(repo, &["worktree", "list", "--porcelain"])
                    .map_err(|e| format!("{}: {}", e.message, e.stderr.trim()))?;
                let paths: Vec<String> = out
                    .lines()
                    .filter_map(|line| line.strip_prefix("worktree "))
                    .map(|path| path.to_string())
                    .collect();
                if paths.is_empty() {
                    return Err(gix_err);
                }
                Ok(paths)
            }
        }
    }

    /// 用 gix 枚举全部 worktree 路径：主工作树 + `<common_dir>/worktrees` 下所有 linked。
    fn worktree_list_gix(&self, repo: &str) -> Result<Vec<String>, String> {
        let repo = gix::discover(repo).map_err(|e| format!("不是 git 仓库: {repo}: {e}"))?;
        let mut paths = Vec::new();
        // 从主仓库读取工作树路径；bare 仓库无主工作树，此时只列 linked。
        if let Ok(main) = repo.main_repo() {
            if let Some(workdir) = main.workdir() {
                paths.push(workdir.to_string_lossy().into_owned());
            }
        }
        let mut linked = repo
            .worktrees()
            .map_err(|e| format!("枚举 worktree 失败: {e}"))?;
        // git worktree list 按 gitdir 路径排序，保持输出稳定。
        linked.sort_by(|a, b| a.git_dir().cmp(b.git_dir()));
        for proxy in linked {
            let base = proxy
                .base()
                .map_err(|e| format!("读取 worktree 路径失败: {e}"))?;
            paths.push(base.to_string_lossy().into_owned());
        }
        Ok(paths)
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
        if gix::discover(repo_cwd).is_ok() {
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

/// worktree 目标目录：`<root>/<仓库目录名>-<5 字符随机串>`。
fn worktree_dir_for(repo: &str, root: &Path) -> Result<PathBuf, String> {
    let workdir = gix::discover(repo)
        .ok()
        .and_then(|r| r.workdir().map(|p| p.to_path_buf()))
        .ok_or_else(|| format!("不是 git 仓库: {repo}"))?;
    let name = workdir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    std::fs::create_dir_all(root).map_err(|e| format!("创建 worktree 根目录失败: {e}"))?;
    Ok(root.join(format!("{name}-{}", worktree_suffix())))
}

/// 5 字符随机后缀；UUID v4 的前 5 个十六进制字符满足目录名简洁性与随机性要求。
fn worktree_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..5].to_string()
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
        let st = r.diff(dir.to_str().unwrap());
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

    /// 文件各 hunk 的内容行（不含前缀），供断言改动文本。
    fn content_of(file: &GitDiffFile) -> String {
        file.hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn diff_includes_untracked_files() {
        let dir = init_repo();
        std::fs::write(dir.join("untracked.txt"), "not staged\n").unwrap();
        let result = GitRunner::new().diff(dir.to_str().unwrap());
        let file = result
            .files
            .iter()
            .find(|file| file.path == "untracked.txt")
            .expect("未跟踪文件应出现在 diff 中");
        assert!(matches!(file.status, GitChangeStatus::Added));
        assert!(content_of(file).contains("not staged"));
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
        let diff = runner.diff(dir.to_str().unwrap());
        let untracked = diff
            .files
            .iter()
            .find(|file| file.path == "untracked-link")
            .expect("未跟踪符号链接应出现在 diff 中");
        assert!(content_of(untracked).contains("secret.txt"));
        assert!(!content_of(untracked).contains("must not leak"));

        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("a.txt")).unwrap();
        let diff = runner.diff(dir.to_str().unwrap());
        let replaced = diff
            .files
            .iter()
            .find(|file| file.path == "a.txt")
            .expect("被符号链接替换的 tracked 文件应出现在 diff 中");
        assert!(content_of(replaced).contains("secret.txt"));
        assert!(!content_of(replaced).contains("must not leak"));
    }

    #[test]
    fn non_repo_marks_not_repo() {
        let dir = unique_dir("amux-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let st = GitRunner::new().diff(dir.to_str().unwrap()).not_repo;
        assert!(st);
    }

    #[test]
    fn diff_returns_structured_files() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\nline3\n").unwrap();
        std::fs::write(dir.join("new.txt"), "hello\nworld\n").unwrap();
        git(&dir, &["add", "new.txt"]);
        let d = GitRunner::new().diff(dir.to_str().unwrap());
        assert!(!d.not_repo);
        let a = d.files.iter().find(|f| f.path == "a.txt").expect("a.txt");
        assert_eq!(a.additions, 2);
        assert_eq!(a.deletions, 1);
        assert!(matches!(a.status, GitChangeStatus::Modified));
        assert_eq!(a.hunks.len(), 1);
        assert!(a.hunks[0].header.starts_with("@@ "));
        assert!(content_of(a).contains("CHANGED"));
        let n = d
            .files
            .iter()
            .find(|f| f.path == "new.txt")
            .expect("new.txt");
        assert!(matches!(n.status, GitChangeStatus::Added));
        assert_eq!(n.additions, 2);
    }

    /// 目录约定：`<worktrees 根>/<仓库目录名>-<5 字符随机串>`。
    #[test]
    fn worktree_dir_follows_amux_convention() {
        let repo = init_repo();
        let root = unique_dir("amux-worktrees");
        let repo_name = repo.file_name().unwrap().to_string_lossy().into_owned();

        let target = worktree_dir_for(repo.to_str().unwrap(), &root).unwrap();
        assert_eq!(target.parent(), Some(root.as_path()));
        let name = target.file_name().unwrap().to_string_lossy().into_owned();
        let suffix = name
            .strip_prefix(&format!("{repo_name}-"))
            .unwrap_or_default();
        assert_eq!(suffix.len(), 5, "{name}");
        assert!(
            suffix.chars().all(|ch| ch.is_ascii_alphanumeric()),
            "{suffix}"
        );
    }

    #[test]
    fn worktree_suffix_is_five_alphanumeric_chars() {
        let suffix = worktree_suffix();
        assert_eq!(suffix.len(), 5);
        assert!(suffix.chars().all(|ch| ch.is_ascii_alphanumeric()));
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
