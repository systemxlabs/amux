//! git 能力：
//! - diff 查询、untracked 判定、分支/引用查询、merge-base：gitoxide（gix）结构化实现，
//!   不依赖 git 二进制、无本地化输出解析
//! - gitoxide 无等价能力处保留 git CLI：worktree 新增/删除（gix-worktree 仅有列出能力，
//!   无增删管理）与远端 fetch（gix 的远端协议栈不完整支持 CLI 级别的配置和助手）
//!
//! 依赖上用 `gix` 伞 crate（DESIGN「Daemon 技术栈 - Git：gix」，只开 status / revision / sha1
//! 三个特性）：这里需要的是「仓库访问 + status + revision 解析」这一整套跨子系统能力，
//! 正是伞 crate 的 status 与 revision 特性所封装的部分；改用细粒度子 crate 得自己复刻
//! 那几百行装配，收益仅是去掉伞 crate 作为非可选依赖带进来的 gix-protocol /
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
    GitBranch, GitBranchKind, GitBranchListResult, GitChangeStatus, GitDiffFile, GitDiffHunk,
    GitDiffLine, GitDiffLineKind, GitDiffResult, WorktreeBase,
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
    pub fn diff(&self, cwd: &str, base: &str) -> Result<GitDiffResult, String> {
        update_remote_tracking_base(cwd, base)?;
        Ok(self.diff_inner(cwd, base))
    }

    fn diff_inner(&self, cwd: &str, base: &str) -> GitDiffResult {
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
        let Ok(head) = repo.head_commit() else {
            return empty();
        };
        let baseline_tree = if base != "HEAD" {
            let Some(merge_base) = merge_base_commit(&repo, base, head.id) else {
                return empty();
            };
            let Ok(baseline_tree) = merge_base.tree() else {
                return empty();
            };
            baseline_tree
        } else {
            let Ok(head_tree) = head.tree() else {
                return empty();
            };
            head_tree
        };

        // 收集变更路径：指定的 merge-base vs HEAD（已提交）+ HEAD vs index（staged）
        // + index vs 工作区（unstaged）。
        // 重命名检测关闭：重命名显示为删除+新增（`git diff --no-renames` 语义）。
        let mut paths = BTreeSet::<BString>::new();
        if base != "HEAD" {
            let Ok(head_tree) = head.tree() else {
                return empty();
            };
            let Ok(mut changes) = baseline_tree.changes() else {
                return empty();
            };
            changes.options(|options| {
                options.track_rewrites(None);
            });
            if changes
                .for_each_to_obtain_tree(&head_tree, |change| {
                    // 目录内容变化时，gix 会同时报告目录级变更。目录没有 blob，
                    // 若当作文件继续处理会生成 +0/-0 的伪文件项。
                    if matches!(
                        change.entry_mode().kind(),
                        gix::object::tree::EntryKind::Blob
                            | gix::object::tree::EntryKind::BlobExecutable
                            | gix::object::tree::EntryKind::Link
                    ) {
                        paths.insert(change.location().into());
                    }
                    Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
                })
                .is_err()
            {
                return empty();
            }
        }
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
            let old = baseline_tree
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
                        Some(fp) => {
                            if fp.hunks.is_empty() {
                                let old_bytes = repo
                                    .find_blob(*old_id)
                                    .map(|blob| blob.data.to_vec())
                                    .unwrap_or_default();
                                if worktree_bytes(&abs).as_deref() == Some(old_bytes.as_slice()) {
                                    continue;
                                }
                            }
                            fp
                        }
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

    /// 查询本地与远端跟踪分支，并标记 worktree 基分支及本地、远端默认分支。
    pub fn branches(&self, cwd: &str) -> Result<GitBranchListResult, String> {
        let repo = gix::discover(cwd).map_err(|error| format!("不是 git 仓库: {cwd}: {error}"))?;
        let current = repo
            .head_name()
            .ok()
            .flatten()
            .map(|name| name.shorten().to_str_lossy().into_owned());
        let worktree_base = worktree_base(&repo);
        let mut local_names = Vec::new();
        let mut branches = Vec::new();
        for reference in repo
            .references()
            .map_err(|error| format!("读取分支失败: {error}"))?
            .local_branches()
            .map_err(|error| format!("读取本地分支失败: {error}"))?
        {
            let reference = reference.map_err(|error| format!("读取本地分支失败: {error}"))?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            local_names.push(name.clone());
            branches.push(GitBranch {
                name,
                full_name: reference.name().to_string(),
                kind: GitBranchKind::Local,
                remote: None,
                remote_branch: None,
                is_worktree_base: false,
                is_local_default: false,
                is_remote_default: false,
            });
        }
        local_names.sort();
        let local_default = default_branch(&repo, &local_names, current.as_deref());
        let remote_defaults = remote_default_branches(&repo);

        for reference in repo
            .references()
            .map_err(|error| format!("读取分支失败: {error}"))?
            .remote_branches()
            .map_err(|error| format!("读取远端跟踪分支失败: {error}"))?
        {
            let reference = reference.map_err(|error| format!("读取远端跟踪分支失败: {error}"))?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            if !name.ends_with("/HEAD") {
                let remote = reference
                    .remote_name(gix::remote::Direction::Fetch)
                    .and_then(|name| name.as_symbol().map(str::to_string));
                let remote_branch = remote.as_ref().and_then(|remote| {
                    name.strip_prefix(&format!("{remote}/")).map(str::to_string)
                });
                branches.push(GitBranch {
                    name,
                    full_name: reference.name().to_string(),
                    kind: GitBranchKind::Remote,
                    remote,
                    remote_branch,
                    is_worktree_base: false,
                    is_local_default: false,
                    is_remote_default: false,
                });
            }
        }
        for branch in &mut branches {
            branch.is_worktree_base = worktree_base
                .as_ref()
                .is_some_and(|base| branch_matches_worktree_base(base, branch));
            branch.is_local_default = branch.kind == GitBranchKind::Local
                && local_default.as_deref() == Some(branch.name.as_str());
            branch.is_remote_default =
                branch.kind == GitBranchKind::Remote && remote_defaults.contains(&branch.name);
        }
        branches.sort_by(|left, right| {
            branch_kind_rank(left.kind)
                .cmp(&branch_kind_rank(right.kind))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(GitBranchListResult {
            current_branch: current.unwrap_or_else(|| "HEAD".to_string()),
            branches,
        })
    }

    /// 新建 worktree：目录约定 `<amux_home>/worktrees/<仓库目录名>-<随机串>/`。
    pub fn create_worktree(&self, repo: &str, base: &WorktreeBase) -> Result<String, String> {
        let target = worktree_dir_for(repo, &amux_common::paths::worktrees_dir())?;
        log::info!(
            "创建 worktree: repo={repo} target={} base={}",
            target.display(),
            base.display_name()
        );
        self.add_worktree(repo, &target, base)?;
        write_worktree_base(&target, base)?;
        log::info!("worktree 创建完成: repo={repo} path={}", target.display());
        Ok(target.to_string_lossy().replace('\\', "/"))
    }

    /// 查询仓库的所有 worktree 路径（主工作树 + linked worktrees）。
    /// gix-worktree 可枚举工作树；若枚举失败则回退到 `git worktree list`。
    pub fn worktrees(&self, repo: &str) -> Result<Vec<String>, String> {
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
        log::info!("删除 worktree: repo={repo} path={path}");
        self.remove_worktree(repo, Path::new(path));
        log::info!("worktree 删除完成: repo={repo} path={path}");
    }

    /// 在 `repo_cwd` 仓库内创建指向 `target` 路径的 git worktree。分支名由目标目录
    /// basename 生成。要求仓库已有提交（unborn HEAD 无法建 worktree）。
    fn add_worktree(
        &self,
        repo_cwd: &str,
        target: &Path,
        base: &WorktreeBase,
    ) -> Result<(), String> {
        let target = target.to_string_lossy();
        let branch = Path::new(target.as_ref())
            .file_name()
            .ok_or_else(|| format!("无效 worktree 路径: {target}"))?
            .to_string_lossy()
            .into_owned();
        let base_ref = match base {
            WorktreeBase::Head => None,
            WorktreeBase::Local { name } => Some(format!("refs/heads/{name}")),
            WorktreeBase::Remote { remote, branch } => {
                run(repo_cwd, &["fetch", "--prune", remote])
                    .map_err(|error| format!("{}: {}", error.message, error.stderr.trim()))?;
                Some(format!("refs/remotes/{remote}/{branch}"))
            }
        };
        let mut args = vec!["worktree", "add", "-b", branch.as_str(), target.as_ref()];
        if let Some(base) = base_ref.as_deref() {
            args.push(base);
        }
        run(repo_cwd, &args)
            .map(|_| ())
            .map_err(|e| format!("{}: {}", e.message, e.stderr.trim()))
    }

    /// 移除 worktree（强制），并 prune 主仓库的残留管理信息。
    /// repo 已不存在时退化为直接删目录。
    fn remove_worktree(&self, repo_cwd: &str, target: &Path) {
        let repo_exists = gix::discover(repo_cwd).is_ok();
        if !target.exists() {
            if repo_exists {
                let _ = run(repo_cwd, &["worktree", "prune"]);
            }
            return;
        }

        let target_str = target.to_string_lossy().into_owned();
        if repo_exists {
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
        if target.exists() {
            if let Err(e) = std::fs::remove_dir_all(target) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::error!("删除 worktree 目录失败 {}: {e}", target.display());
                }
            }
        }
        if repo_exists {
            let _ = run(repo_cwd, &["worktree", "prune"]);
        }
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

const WORKTREE_BASE_FILE: &str = "amux-base-branch";

fn branch_kind_rank(kind: GitBranchKind) -> u8 {
    match kind {
        GitBranchKind::Local => 0,
        GitBranchKind::Remote => 1,
    }
}

fn branch_matches_worktree_base(base: &WorktreeBase, branch: &GitBranch) -> bool {
    match base {
        WorktreeBase::Head => false,
        WorktreeBase::Local { name } => branch.kind == GitBranchKind::Local && branch.name == *name,
        WorktreeBase::Remote {
            remote,
            branch: name,
        } => {
            branch.kind == GitBranchKind::Remote
                && branch.remote.as_deref() == Some(remote)
                && branch.remote_branch.as_deref() == Some(name)
        }
    }
}

/// 读取 linked worktree 的基点标记。
fn worktree_base(repo: &gix::Repository) -> Option<WorktreeBase> {
    if repo.kind() != gix::repository::Kind::LinkedWorkTree {
        return None;
    }
    let marker = std::fs::read_to_string(repo.git_dir().join(WORKTREE_BASE_FILE)).ok()?;
    let base = serde_json::from_str::<WorktreeBase>(&marker).ok()?;
    (!matches!(base, WorktreeBase::Head)).then_some(base)
}

fn write_worktree_base(worktree: &Path, base: &WorktreeBase) -> Result<(), String> {
    let repo = gix::discover(worktree)
        .map_err(|error| format!("不是 git 仓库: {}: {error}", worktree.display()))?;
    let marker = serde_json::to_string(base)
        .map_err(|error| format!("序列化 worktree 基点失败: {error}"))?;
    std::fs::write(repo.git_dir().join(WORKTREE_BASE_FILE), marker)
        .map_err(|error| format!("写入 worktree 基分支失败: {error}"))
}

/// 5 字符随机后缀；UUID v4 的前 5 个十六进制字符满足目录名简洁性与随机性要求。
fn worktree_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..5].to_string()
}

/// 把 `base` 视为任意修订表达式解析为提交，再计算它与 HEAD 的最佳 merge-base。
fn merge_base_commit<'a>(
    repo: &'a gix::Repository,
    base: &str,
    head: gix::hash::ObjectId,
) -> Option<gix::Commit<'a>> {
    let base_commit = repo
        .rev_parse_single(base)
        .ok()?
        .object()
        .ok()?
        .peel_to_commit()
        .ok()?;
    let merge_base = repo.merge_base(base_commit.id, head).ok()?;
    merge_base.object().ok()?.into_commit().into()
}

/// 远端跟踪基准在比较前必须通过其所属远端刷新；本地分支和任意修订表达式不触发网络操作。
fn update_remote_tracking_base(cwd: &str, base: &str) -> Result<(), String> {
    let repo = match gix::discover(cwd) {
        Ok(repo) => repo,
        Err(_) => return Ok(()),
    };
    let Some(remote) = remote_for_tracking_branch(&repo, base) else {
        return Ok(());
    };
    run(cwd, &["fetch", "--prune", remote.as_str()])
        .map(|_| ())
        .map_err(|error| format!("{}: {}", error.message, error.stderr.trim()))
}

/// 若 `base` 是现有远端跟踪引用，返回其所属远端名称。
fn remote_for_tracking_branch(repo: &gix::Repository, base: &str) -> Option<String> {
    // base 可能是短名（origin/master）或完整引用名（refs/remotes/origin/master）
    let reference = if base.starts_with("refs/") {
        base.to_string()
    } else {
        format!("refs/remotes/{base}")
    };
    let refs = repo.references().ok()?;
    let mut names = refs.prefixed("refs/remotes/").ok()?;
    if !names.by_ref().any(|entry| {
        entry
            .map(|item| item.name().to_string() == reference)
            .unwrap_or(false)
    }) {
        return None;
    }

    repo.remote_names()
        .into_iter()
        .filter(|remote| reference.strip_prefix(&format!("refs/remotes/{remote}/")).is_some())
        // 远端名可包含 `/`，优先选择最长匹配前缀。
        .max_by_key(|remote| remote.len())
        .map(|remote| remote.to_string())
}

/// 默认分支优先取远端 origin/HEAD，其次取常见的 main/master，最后回退当前分支。
fn default_branch(
    repo: &gix::Repository,
    branches: &[String],
    current: Option<&str>,
) -> Option<String> {
    if let Some(target) = repo
        .try_find_reference("refs/remotes/origin/HEAD")
        .ok()
        .flatten()
        .and_then(|reference| reference.follow().transpose().ok().flatten())
        .and_then(|reference| reference.target().try_name().map(|name| name.to_string()))
        .and_then(|name| {
            name.strip_prefix("refs/remotes/origin/")
                .map(str::to_string)
        })
    {
        let target_commit = repo
            .rev_parse_single(target.as_str())
            .ok()?
            .object()
            .ok()?
            .peel_to_commit()
            .ok()?
            .id;
        if branches.iter().any(|branch| {
            Some(target_commit)
                == repo
                    .rev_parse_single(branch.as_str())
                    .ok()
                    .map(|id| id.detach())
        }) {
            if branches.iter().any(|branch| branch == &target) {
                return Some(target);
            }
        }
    }
    for candidate in ["main", "master"] {
        if branches.iter().any(|branch| branch == candidate) {
            return Some(candidate.to_string());
        }
    }
    current
        .filter(|name| branches.iter().any(|branch| branch == name))
        .map(str::to_string)
}

/// 每个远端的 HEAD 引用指向各自的默认分支；仅依赖引用本身，不依赖 remote 配置。
fn remote_default_branches(repo: &gix::Repository) -> BTreeSet<String> {
    let mut defaults = BTreeSet::new();
    let refs = match repo.references().ok() {
        Some(refs) => refs,
        None => return defaults,
    };
    let remotes: Vec<String> = match refs.prefixed("refs/remotes/").ok() {
        Some(iter) => iter
            .filter_map(|reference| reference.ok())
            .filter_map(|reference| {
                let name = reference.name().to_string();
                let short = name.strip_prefix("refs/remotes/")?;
                // refs/remotes/<remote>/HEAD 中 remote 部分是第一个路径段
                short
                    .strip_suffix("/HEAD")?
                    .split('/')
                    .next()
                    .map(str::to_string)
            })
            .collect(),
        None => return defaults,
    };
    for remote in remotes {
        let head_name = format!("refs/remotes/{remote}/HEAD");
        let Some(head_ref) = repo.try_find_reference(head_name.as_str()).ok().flatten() else {
            continue;
        };
        let Some(target_ref) = follow_symbolic_reference(head_ref) else {
            continue;
        };
        let Some(commit_id) = target_ref
            .target()
            .try_id()
            .map(|id| gix::hash::ObjectId::from(id))
        else {
            continue;
        };
        let prefix = format!("refs/remotes/{remote}/");
        let refs = match repo.references().ok() {
            Some(refs) => refs,
            None => continue,
        };
        let mut iter = match refs.prefixed(prefix.as_str()).ok() {
            Some(iter) => iter,
            None => continue,
        };
        if let Some(target_name) = iter
            .by_ref()
            .filter_map(|reference| reference.ok())
            .find_map(|reference| {
                let name = reference.name().to_string();
                if name == format!("{prefix}HEAD") {
                    return None;
                }
                let id = reference
                    .target()
                    .try_id()
                    .map(|id| gix::hash::ObjectId::from(id))?;
                (id == commit_id).then_some(name)
            })
        {
            if let Some(name) = target_name.strip_prefix(prefix.as_str()) {
                defaults.insert(format!("{remote}/{name}"));
            }
        }
    }
    defaults
}

/// 跟随符号引用直到找到最终引用；非符号引用则返回自身。
fn follow_symbolic_reference(reference: gix::Reference<'_>) -> Option<gix::Reference<'_>> {
    if matches!(reference.target().kind(), gix::reference::Kind::Symbolic) {
        reference.follow()?.ok()
    } else {
        Some(reference)
    }
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
        git(&dir, &["init", "-q"]);
        git(&dir, &["checkout", "-b", "main", "-q"]);
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
        let st = r.diff(dir.to_str().unwrap(), "HEAD").unwrap();
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
        let result = GitRunner::new()
            .diff(dir.to_str().unwrap(), "HEAD")
            .unwrap();
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
        let diff = runner.diff(dir.to_str().unwrap(), "HEAD").unwrap();
        let untracked = diff
            .files
            .iter()
            .find(|file| file.path == "untracked-link")
            .expect("未跟踪符号链接应出现在 diff 中");
        assert!(content_of(untracked).contains("secret.txt"));
        assert!(!content_of(untracked).contains("must not leak"));

        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("a.txt")).unwrap();
        let diff = runner.diff(dir.to_str().unwrap(), "HEAD").unwrap();
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
        let st = GitRunner::new()
            .diff(dir.to_str().unwrap(), "HEAD")
            .unwrap()
            .not_repo;
        assert!(st);
    }

    #[test]
    fn diff_returns_structured_files() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\nline3\n").unwrap();
        std::fs::write(dir.join("new.txt"), "hello\nworld\n").unwrap();
        git(&dir, &["add", "new.txt"]);
        let d = GitRunner::new()
            .diff(dir.to_str().unwrap(), "HEAD")
            .unwrap();
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

    #[test]
    fn diff_uses_merge_base_of_requested_branch_and_head() {
        let dir = init_repo();
        git(&dir, &["branch", "base"]);
        std::fs::write(dir.join("a.txt"), "line1\nmain change\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-m", "main change", "-q"]);

        git(&dir, &["checkout", "base", "-q"]);
        std::fs::write(dir.join("base-only.txt"), "base only\n").unwrap();
        git(&dir, &["add", "base-only.txt"]);
        git(&dir, &["commit", "-m", "base change", "-q"]);
        git(&dir, &["checkout", "main", "-q"]);

        let diff = GitRunner::new()
            .diff(dir.to_str().unwrap(), "base")
            .unwrap();
        assert!(
            diff.files.iter().any(|file| file.path == "a.txt"),
            "应包含基准分支 merge-base 之后的 HEAD 提交内容: {diff:?}"
        );
        assert!(
            !diff.files.iter().any(|file| file.path == "base-only.txt"),
            "不应反向包含基准分支独有提交: {diff:?}"
        );
    }

    #[test]
    fn diff_omits_files_reverted_to_selected_base() {
        let dir = init_repo();
        git(&dir, &["branch", "base"]);
        std::fs::write(dir.join("a.txt"), "line1\ncommitted change\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-m", "change", "-q"]);
        std::fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();

        let diff = GitRunner::new()
            .diff(dir.to_str().unwrap(), "base")
            .unwrap();
        assert!(
            !diff.files.iter().any(|file| file.path == "a.txt"),
            "恢复到基准内容后不应显示空 diff: {diff:?}"
        );
    }

    #[test]
    fn diff_remote_tracking_branch_fetches_latest_base() {
        let repo = init_repo();
        let remote = unique_dir("amux-diff-remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        let stale = git(&repo, &["rev-parse", "origin/main"]).trim().to_string();

        let producer = unique_dir("amux-diff-producer");
        git(
            remote.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--branch",
                "main",
                remote.to_str().unwrap(),
                producer.to_str().unwrap(),
            ],
        );
        git(&producer, &["config", "user.email", "t@t"]);
        git(&producer, &["config", "user.name", "t"]);
        std::fs::write(producer.join("latest.txt"), "latest\n").unwrap();
        git(&producer, &["add", "."]);
        git(&producer, &["commit", "-m", "latest", "-q"]);
        git(&producer, &["push", "-q"]);
        let latest = git(&producer, &["rev-parse", "HEAD"]).trim().to_string();
        assert_ne!(latest, stale);

        GitRunner::new()
            .diff(repo.to_str().unwrap(), "origin/main")
            .unwrap();
        assert_eq!(git(&repo, &["rev-parse", "origin/main"]).trim(), latest);

        let _ = std::fs::remove_dir_all(&producer);
        let _ = std::fs::remove_dir_all(&remote);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn diff_returns_error_when_remote_tracking_base_cannot_be_updated() {
        let repo = init_repo();
        let missing = unique_dir("amux-missing-remote");
        git(
            &repo,
            &["remote", "add", "origin", missing.to_str().unwrap()],
        );
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        let error = GitRunner::new()
            .diff(repo.to_str().unwrap(), "origin/main")
            .unwrap_err();
        assert!(error.contains("git fetch --prune origin 失败"), "{error}");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn diff_omits_directory_entries_for_directory_rename() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.join("before/nested")).unwrap();
        std::fs::write(dir.join("before/nested/file.txt"), "content\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "add directory", "-q"]);
        git(&dir, &["mv", "before", "after"]);
        git(&dir, &["commit", "-m", "rename directory", "-q"]);

        let diff = GitRunner::new()
            .diff(dir.to_str().unwrap(), "HEAD~1")
            .unwrap();
        assert!(
            !diff
                .files
                .iter()
                .any(|file| file.path == "before" || file.path == "after"),
            "目录不是文件，不应出现在 diff.files 中: {diff:?}"
        );
        assert!(
            diff.files
                .iter()
                .any(|file| file.path == "before/nested/file.txt"),
            "旧目录下的文件删除应出现在 diff 中: {diff:?}"
        );
        assert!(
            diff.files
                .iter()
                .any(|file| file.path == "after/nested/file.txt"),
            "新目录下的文件新增应出现在 diff 中: {diff:?}"
        );
    }

    #[test]
    fn branches_mark_default_and_worktree_base() {
        let repo = init_repo();
        git(&repo, &["branch", "feature"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let runner = GitRunner::new();
        let branches = runner.branches(repo.to_str().unwrap()).unwrap();
        assert_eq!(branches.current_branch, "main");
        let main = branches
            .branches
            .iter()
            .find(|branch| branch.name == "main")
            .unwrap();
        assert!(main.is_local_default);
        assert_eq!(main.kind, GitBranchKind::Local);
        assert_eq!(main.full_name, "refs/heads/main");
        assert!(
            branches.branches.iter().any(|branch| {
                branch.name == "origin/main"
                    && branch.full_name == "refs/remotes/origin/main"
                    && branch.kind == GitBranchKind::Remote
                    && branch.remote.as_deref() == Some("origin")
                    && branch.remote_branch.as_deref() == Some("main")
                    && branch.is_remote_default
            }),
            "{branches:?}"
        );

        let root = unique_dir("amux-branches-worktree");
        let target = worktree_dir_for(repo.to_str().unwrap(), &root).unwrap();
        let feature_base = WorktreeBase::Local {
            name: "feature".into(),
        };
        runner
            .add_worktree(repo.to_str().unwrap(), &target, &feature_base)
            .unwrap();
        write_worktree_base(&target, &feature_base).unwrap();
        let branches = runner.branches(target.to_str().unwrap()).unwrap();
        assert_eq!(
            branches.current_branch,
            target.file_name().unwrap().to_string_lossy(),
            "worktree 应报告自身检出的分支，而不是创建基分支"
        );
        let feature = branches
            .branches
            .iter()
            .find(|branch| branch.name == "feature")
            .unwrap();
        assert!(feature.is_worktree_base);

        runner.worktree_remove(repo.to_str().unwrap(), target.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&root);
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

        git.add_worktree(repo_str, &target, &WorktreeBase::Head)
            .unwrap();
        assert!(target.join(".git").exists(), "worktree 应为有效检出");

        let listed = git.worktrees(repo_str).unwrap();
        assert!(
            listed.iter().any(|path| path.ends_with(&target_name)),
            "worktrees 应包含新建目录: {listed:?}"
        );

        git.worktree_remove(repo_str, target.to_str().unwrap());
        assert!(!target.exists(), "remove 后目录应消失");
        let listed = git.worktrees(repo_str).unwrap();
        assert!(
            !listed.iter().any(|path| path.ends_with(&target_name)),
            "移除后不应仍在列表中: {listed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_worktree_fetches_latest_base() {
        let repo = init_repo();
        let remote = unique_dir("amux-worktree-remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        let stale = git(&repo, &["rev-parse", "origin/main"]).trim().to_string();

        let producer = unique_dir("amux-worktree-producer");
        git(
            remote.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--branch",
                "main",
                remote.to_str().unwrap(),
                producer.to_str().unwrap(),
            ],
        );
        git(&producer, &["config", "user.email", "t@t"]);
        git(&producer, &["config", "user.name", "t"]);
        std::fs::write(producer.join("latest.txt"), "latest\n").unwrap();
        git(&producer, &["add", "."]);
        git(&producer, &["commit", "-m", "latest", "-q"]);
        git(&producer, &["push", "-q"]);
        let latest = git(&producer, &["rev-parse", "HEAD"]).trim().to_string();
        assert_ne!(latest, stale);

        let root = unique_dir("amux-worktree-remote-check");
        let target = worktree_dir_for(repo.to_str().unwrap(), &root).unwrap();
        let runner = GitRunner::new();
        runner
            .add_worktree(
                repo.to_str().unwrap(),
                &target,
                &WorktreeBase::Remote {
                    remote: "origin".into(),
                    branch: "main".into(),
                },
            )
            .unwrap();
        assert_eq!(git(&target, &["rev-parse", "HEAD"]).trim(), latest);

        runner.worktree_remove(repo.to_str().unwrap(), target.to_str().unwrap());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(producer);
        let _ = std::fs::remove_dir_all(remote);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn worktree_remove_prunes_missing_directory_and_is_idempotent() {
        let repo = init_repo();
        let repo_str = repo.to_str().unwrap();
        let root = unique_dir("amux-worktree-idempotent");
        let runner = GitRunner::new();
        let target = worktree_dir_for(repo_str, &root).unwrap();
        let target_name = target.file_name().unwrap().to_string_lossy().into_owned();

        runner
            .add_worktree(repo_str, &target, &WorktreeBase::Head)
            .unwrap();
        std::fs::remove_dir_all(&target).unwrap();

        runner.worktree_remove(repo_str, target.to_str().unwrap());
        runner.worktree_remove(repo_str, target.to_str().unwrap());

        let listed = runner.worktrees(repo_str).unwrap();
        assert!(
            !listed.iter().any(|path| path.ends_with(&target_name)),
            "清理缺失目录后不应残留 worktree 注册项: {listed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
