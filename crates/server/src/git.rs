//! git 能力：
//! - diff 查询、untracked 判定：gitoxide（gix）结构化实现，不依赖 git 二进制、
//!   无本地化输出解析
//! - gitoxide 无等价能力处保留 git CLI：patch 应用（`git apply --reverse`）、
//!   索引+工作区整体恢复（`git restore` / `git clean`）

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::diff::blob::pipeline::{Mode, WorktreeRoots};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{ResourceKind, UnifiedDiff};

use protocol::{
    GitChangeStatus, GitDiffFile, GitDiffHunk, OpResult, WorkspaceDiffResult, WorkspaceEntry,
    WorkspaceListResult, WorkspaceReadResult,
};

#[derive(Default)]
pub struct GitRunner;

/// git CLI 输出错误（restore 的 CLI 兜底路径用）。
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
fn op_err(message: impl Into<String>) -> OpResult {
    OpResult {
        ok: false,
        message: Some(message.into()),
    }
}

/// 把相对 cwd 的路径换算为仓库根相对路径（cwd 通常即仓库根，映射为恒等）。
fn repo_relative_path(workdir: &Path, cwd: &str, p: &str) -> String {
    match Path::new(cwd).strip_prefix(workdir) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.join(p).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}

/// 把仓库根相对路径换算为相对 cwd 的路径——diff 结果的 path 与 workspace.list/read
/// 同基准（相对 cwd），GUI 侧才能把 diff 文件直接喂回浏览/读取接口。
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

/// 单文件 unified diff 汇总：完整 patch 文本 + 按 hunk 拆分的可独立反向应用 patch。
struct FilePatch {
    patch: String,
    hunks: Vec<GitDiffHunk>,
    additions: u32,
    deletions: u32,
}

/// 统一 diff 渲染收集器：UnifiedDiff 逐个 hunk 回调，收集行级数据用于自拼 patch 文本。
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

/// 逐行渲染 hunk 体（前缀 + 行内容 + 换行）。
fn hunk_body_text(lines: &[(DiffLineKind, Vec<u8>)]) -> String {
    let mut out = String::new();
    for (kind, content) in lines {
        let prefix = match kind {
            DiffLineKind::Context => ' ',
            DiffLineKind::Add => '+',
            DiffLineKind::Remove => '-',
        };
        out.push(prefix);
        out.push_str(&String::from_utf8_lossy(content));
        out.push('\n');
    }
    out
}

/// 按行数（以 `\n` 计）与每行内容构造整文件新增/删除 patch（空的一侧只有一个 hunk）。
fn whole_file_patch(path: &str, bytes: &[u8], added: bool) -> FilePatch {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let n = lines.len() as u32;
    let header = if added {
        format!("diff --git a/{path} b/{path}\n--- /dev/null\n+++ b/{path}\n")
    } else {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ /dev/null\n")
    };
    let hunk_hdr = if added {
        format!("@@ -0,0 +1,{n} @@\n")
    } else {
        format!("@@ -1,{n} +0,0 @@\n")
    };
    let prefix = if added { '+' } else { '-' };
    let body: String = lines.iter().map(|l| format!("{prefix}{l}\n")).collect();
    let patch = format!("{header}{hunk_hdr}{body}");
    let hunk_patch = patch.clone();
    let hunks = vec![GitDiffHunk {
        header: hunk_hdr.trim_end().to_string(),
        patch: hunk_patch,
    }];
    FilePatch {
        patch,
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
    old_mode: gix::object::tree::EntryMode,
    path: &BStr,
) -> Option<FilePatch> {
    let new_id = gix::hash::ObjectId::null(repo.object_hash());
    cache
        .set_resource(
            old_id,
            old_mode.into(),
            path,
            ResourceKind::OldOrSource,
            &repo.objects,
        )
        .ok()?;
    cache
        .set_resource(
            new_id,
            old_mode.into(),
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
        // 二进制等不可行内 diff 的资源：无 hunk，仅文件头（与 `git diff` 无内容时的表现一致）。
        Operation::SourceOrDestinationIsBinary => Vec::new(),
        Operation::ExternalCommand { .. } => unreachable!("内部 diff 选项已强制，不应走外部命令"),
    };
    let header = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
    let mut patch = header;
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
        let hunk_text = format!("{header}\n{}", hunk_body_text(&lines));
        hunks.push(GitDiffHunk {
            header: header.clone(),
            patch: format!("{}{}", patch, hunk_text),
        });
        patch.push_str(&hunk_text);
    }
    Some(FilePatch {
        patch,
        hunks,
        additions,
        deletions,
    })
}

impl GitRunner {
    pub fn new() -> Self {
        GitRunner
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

    /// 结构化 diff：gitoxide 实现。
    /// cwd 非 git 仓库时返回 `not_repo` 标记。
    pub fn diff(&self, cwd: &str, path: Option<&str>) -> WorkspaceDiffResult {
        let empty = || WorkspaceDiffResult {
            files: Vec::new(),
            not_repo: false,
        };
        let repo = match gix::discover(cwd) {
            Ok(r) => r,
            // 非仓库（向上查找无 .git）或仓库不可用时，按 workspace.diff 协议返回 not_repo。
            Err(_) => {
                return WorkspaceDiffResult {
                    files: Vec::new(),
                    not_repo: true,
                };
            }
        };
        // bare 仓库没有工作区，diff 无意义（`git diff` 同场景报错）
        let Some(workdir) = repo.workdir() else {
            return WorkspaceDiffResult {
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

        if let Some(f) = path.map(|p| repo_relative_path(workdir, cwd, p)) {
            let f = BString::from(f);
            let prefix = format!("{f}/");
            paths.retain(|p| p == &f || p.as_bytes().starts_with(prefix.as_bytes()));
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
            let new_exists = abs.exists();
            let fp = match (&old, new_exists) {
                (Some((old_id, old_mode)), true) => {
                    let fp = modified_patch(&repo, &mut cache, *old_id, *old_mode, p.as_ref());
                    // blob diff 资源缓存只增不减，逐文件释放以免大 diff 时内存无界增长
                    cache.clear_resource_cache_keep_allocation();
                    match fp {
                        Some(fp) => fp,
                        None => continue,
                    }
                }
                (None, true) => {
                    let bytes = std::fs::read(&abs).unwrap_or_default();
                    whole_file_patch(&p_str, &bytes, true)
                }
                (Some((old_id, _)), false) => {
                    let bytes = repo
                        .find_blob(*old_id)
                        .map(|b| b.data.to_vec())
                        .unwrap_or_default();
                    whole_file_patch(&p_str, &bytes, false)
                }
                (None, false) => continue,
            };
            let status = match (&old, new_exists) {
                (None, true) => GitChangeStatus::Added,
                (Some(_), false) => GitChangeStatus::Deleted,
                _ => GitChangeStatus::Modified,
            };
            files.push(GitDiffFile {
                path: cwd_relative_path(workdir, cwd, &p_str),
                status,
                additions: fp.additions,
                deletions: fp.deletions,
                patch: fp.patch,
                hunks: fp.hunks,
            });
        }
        WorkspaceDiffResult {
            files,
            not_repo: false,
        }
    }

    /// 撤销工作区变更。
    /// - `patch`：单 hunk/单文件 patch 反向应用——gitoxide 无 patch 应用引擎，保留 `git apply --reverse`
    ///   （diff patch 的 a/ b/ 头为仓库根相对路径，故从仓库根执行 apply，cwd 为子目录时同样正确）
    /// - `path`：单文件——tracked 用 `git restore`；untracked 直接删除（从未提交，revert = 移除）
    /// - 都不给：全部变更——`git restore` 全部 tracked 变更 + `git clean` 全部 untracked
    pub fn restore(&self, cwd: &str, path: Option<&str>, patch: Option<&str>) -> OpResult {
        if let Some(p) = patch {
            // 唯一临时目录（并发 revert 不互相覆盖；uuid v4）
            let dir = std::env::temp_dir().join(format!("amux-revert-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).ok();
            let patch_file = dir.join("revert.patch");
            if std::fs::write(&patch_file, p).is_err() {
                return op_err("写入 patch 失败");
            }
            // patch 路径以仓库根为基准：从仓库根应用（cwd 为其子目录时也正确）
            let apply_dir = gix::discover(cwd)
                .ok()
                .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from(cwd));
            return match run(
                apply_dir.to_string_lossy().as_ref(),
                &["apply", "--reverse", patch_file.to_string_lossy().as_ref()],
            ) {
                Ok(_) => OpResult {
                    ok: true,
                    message: None,
                },
                Err(e) => op_err(e.stderr.trim()),
            };
        }
        if let Some(target) = path {
            let target = match validate_restore_path(cwd, target) {
                Ok(target) => target,
                Err(error) => return error,
            };
            // untracked：从未提交，revert = 删除工作区文件
            if self.is_untracked(cwd, &target) {
                return match std::fs::remove_file(std::path::Path::new(cwd).join(&target)) {
                    Ok(_) => OpResult {
                        ok: true,
                        message: None,
                    },
                    Err(e) => op_err(format!("删除 untracked 文件失败: {e}")),
                };
            }
            return match run(cwd, &["restore", "--staged", "--worktree", "--", &target]) {
                Ok(_) => OpResult {
                    ok: true,
                    message: None,
                },
                Err(e) => op_err(e.stderr.trim()),
            };
        }
        if let Err(e) = run(cwd, &["restore", "--staged", "--worktree", "--", "."]) {
            return op_err(e.stderr.trim());
        }
        match run(cwd, &["clean", "-fd"]) {
            Ok(_) => OpResult {
                ok: true,
                message: None,
            },
            Err(e) => OpResult {
                ok: false,
                message: Some(e.stderr.trim().to_string()),
            },
        }
    }

    /// 目标路径是否 untracked（gitoxide 判定：既不在 HEAD 树也不在索引中）。
    fn is_untracked(&self, cwd: &str, target: &str) -> bool {
        let Ok(repo) = gix::discover(cwd) else {
            return false;
        };
        let Some(workdir) = repo.workdir() else {
            return false;
        };
        let full = BString::from(repo_relative_path(workdir, cwd, target));
        let in_head = repo
            .head_tree()
            .ok()
            .and_then(|t| {
                t.lookup_entry_by_path(gix::path::from_bstr(&full))
                    .ok()
                    .flatten()
            })
            .is_some();
        if in_head {
            return false;
        }
        let in_index = repo
            .index_or_empty()
            .ok()
            // worktree::Index = Arc<SharedFileSnapshot<File>>，方法解析自动解引用到 State
            .map(|idx| idx.entry_by_path(full.as_bytes().as_bstr()).is_some())
            .unwrap_or(false);
        !in_index
    }

    /// 在 `repo_cwd` 仓库内创建指向 `target` 路径的 git worktree（docs/DESIGN.md
    /// 「工作树存储」）。分支名由 git 取目标目录 basename 自动生成。要求仓库已有
    /// 提交（unborn HEAD 无法建 worktree）。
    pub fn create_worktree(&self, repo_cwd: &str, target: &Path) -> Result<(), String> {
        run(
            repo_cwd,
            &["worktree", "add", target.to_string_lossy().as_ref()],
        )
        .map(|_| ())
        .map_err(|e| format!("{}: {}", e.message, e.stderr.trim()))
    }

    /// 移除 worktree（强制：会话删除级联清理不因未提交改动而失败），并 prune
    /// 主仓库的残留管理信息。repo 已不存在时退化为直接删目录。
    pub fn remove_worktree(&self, repo_cwd: &str, target: &Path) {
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

    /// 判定 cwd 是否为 git 仓库（worktree 开关的前置校验）。
    pub fn is_repo(&self, cwd: &str) -> bool {
        gix::discover(cwd)
            .ok()
            .and_then(|r| r.workdir().map(|_| ()))
            .is_some()
    }
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
    let candidate = root.join(relative);
    if candidate.exists() {
        let canonical = match candidate.canonicalize() {
            Ok(path) => path,
            Err(error) => return Err(op_err(format!("工作目录路径不可访问: {error}"))),
        };
        if !canonical.starts_with(&root) {
            return Err(op_err("工作目录路径超出工作目录范围"));
        }
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn canonical_workspace_root(cwd: &str) -> Result<PathBuf, String> {
    let root = Path::new(cwd)
        .canonicalize()
        .map_err(|e| format!("工作目录不可访问: {e}"))?;
    if !root.is_dir() {
        return Err(format!("工作目录不是文件夹: {}", root.display()));
    }
    Ok(root)
}

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

    #[test]
    fn non_repo_marks_not_repo() {
        let dir = unique_dir("amux-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let st = GitRunner::new().diff(dir.to_str().unwrap(), None).not_repo;
        assert!(st);
    }

    #[test]
    fn workspace_list_sorts_directories_and_paginates() {
        let dir = unique_dir("amux-workspace-list");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("z.txt"), "z").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let result = GitRunner::new()
            .list_workspace(dir.to_str().unwrap(), None, 2, 0)
            .unwrap();
        assert_eq!(result.path, "");
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].name, "src");
        assert_eq!(result.entries[1].name, "a.txt");
        assert!(result.has_more);
        assert_eq!(result.next_offset, 2);

        let next = GitRunner::new()
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
    fn workspace_read_returns_line_pages() {
        let dir = unique_dir("amux-workspace-read");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        let runner = GitRunner::new();

        let first = runner
            .read_workspace(dir.to_str().unwrap(), "notes.txt", 0, 2)
            .unwrap();
        assert_eq!(first.content, "one\ntwo\n");
        assert!(first.has_more);
        assert_eq!(first.next_offset, 2);

        let second = runner
            .read_workspace(dir.to_str().unwrap(), "notes.txt", first.next_offset, 2)
            .unwrap();
        assert_eq!(second.content, "three\n");
        assert!(!second.has_more);
    }

    #[test]
    fn workspace_paths_cannot_escape_root() {
        let dir = unique_dir("amux-workspace-safe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inside.txt"), "inside").unwrap();
        let runner = GitRunner::new();
        let cwd = dir.to_str().unwrap();

        assert!(runner.list_workspace(cwd, Some("../"), 10, 0).is_err());
        assert!(runner.read_workspace(cwd, "/etc/passwd", 0, 10).is_err());

        #[cfg(unix)]
        {
            let outside = unique_dir("amux-workspace-outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.txt"), "secret").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            assert!(runner
                .read_workspace(cwd, "link/secret.txt", 0, 10)
                .is_err());
        }
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
}
