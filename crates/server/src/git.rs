//! git 能力（docs/DESIGN.md「workspace.diff」「workspace.restore」）：
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

use protocol::{GitChangeStatus, GitDiffFile, GitDiffHunk, OpResult, WorkspaceDiffResult};

pub struct GitRunner;

/// git CLI 输出错误（restore 的 CLI 兜底路径用）。
#[derive(Debug)]
pub struct GitError {
    pub message: String,
    #[allow(dead_code)]
    pub stdout: String,
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
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Err(e) => Err(GitError {
            message: format!("git 执行失败: {e}"),
            stdout: String::new(),
            stderr: String::new(),
        }),
    }
}

/// 把相对 cwd 的路径换算为仓库根相对路径（cwd 通常即仓库根，映射为恒等）。
fn repo_relative_path(workdir: &Path, cwd: &str, p: &str) -> String {
    match Path::new(cwd).strip_prefix(workdir) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.join(p).to_string_lossy().into_owned(),
        _ => p.to_string(),
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
#[derive(Default)]
struct HunkCollector {
    hunks: Vec<(HunkHeader, Vec<(DiffLineKind, Vec<u8>)>)>,
}

impl ConsumeHunk for HunkCollector {
    type Out = Vec<(HunkHeader, Vec<(DiffLineKind, Vec<u8>)>)>;

    fn consume_hunk(&mut self, header: HunkHeader, lines: &[(DiffLineKind, &[u8])]) -> std::io::Result<()> {
        self.hunks
            .push((header, lines.iter().map(|(k, l)| (*k, l.to_vec())).collect()));
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
        .set_resource(old_id, old_mode.into(), path, ResourceKind::OldOrSource, &repo.objects)
        .ok()?;
    cache
        .set_resource(new_id, old_mode.into(), path, ResourceKind::NewOrDestination, &repo.objects)
        .ok()?;
    let prep = cache.prepare_diff().ok()?;
    let hunks_data = match prep.operation {
        Operation::InternalDiff { algorithm } => {
            let input = prep.interned_input();
            let diff = gix::diff::blob::diff_with_slider_heuristics(algorithm, &input);
            let ud = UnifiedDiff::new(&diff, &input, HunkCollector::default(), ContextSize::symmetrical(3));
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
        additions += lines.iter().filter(|(k, _)| *k == DiffLineKind::Add).count() as u32;
        deletions += lines.iter().filter(|(k, _)| *k == DiffLineKind::Remove).count() as u32;
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

    /// 结构化 diff（docs/DESIGN.md「workspace.diff」）：gitoxide 实现。
    /// cwd 非 git 仓库时返回 `not_repo` 标记。
    pub fn diff(&self, cwd: &str, path: Option<&str>) -> WorkspaceDiffResult {
        let empty = || WorkspaceDiffResult {
            files: Vec::new(),
            not_repo: false,
        };
        let repo = match gix::discover(cwd) {
            Ok(r) => r,
            // 非仓库（向上查找无 .git）或仓库不可用 → 与旧实现 `rev-parse` 失败一致
            Err(_) => {
                return WorkspaceDiffResult {
                    files: Vec::new(),
                    not_repo: true,
                }
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
        let status = match repo
            .status(gix::progress::Discard)
            .map(|s| {
                s.untracked_files(gix::status::UntrackedFiles::None)
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
                Ok(gix::status::Item::IndexWorktree(gix::status::index_worktree::Item::Modification {
                    rela_path,
                    ..
                })) => {
                    paths.insert(rela_path);
                }
                Ok(_) => {}
                Err(_) => return empty(),
            }
        }

        // 单路径过滤（相对 cwd 的路径映射到仓库根）
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
                path: p_str,
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

    /// 撤销工作区变更（docs/DESIGN.md「workspace.restore」）。
    /// - `patch`：单 hunk/单文件 patch 反向应用——gitoxide 无 patch 应用引擎，保留 `git apply --reverse`
    /// - `path`：单文件——tracked 用 `git restore`；untracked 直接删除（从未提交，revert = 移除）
    /// - 都不给：全部变更——`git restore` 全部 tracked 变更 + `git clean` 全部 untracked
    pub fn restore(&self, cwd: &str, path: Option<&str>, patch: Option<&str>) -> OpResult {
        if let Some(p) = patch {
            // 唯一临时目录（并发 revert 不互相覆盖；uuid v4）
            let dir = std::env::temp_dir().join(format!("amux-revert-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).ok();
            let patch_file = dir.join("revert.patch");
            if std::fs::write(&patch_file, p).is_err() {
                return OpResult {
                    ok: false,
                    message: Some("写入 patch 失败".into()),
                };
            }
            return match run(cwd, &["apply", "--reverse", &patch_file.to_string_lossy()]) {
                Ok(_) => OpResult {
                    ok: true,
                    message: None,
                },
                Err(e) => OpResult {
                    ok: false,
                    message: Some(e.stderr.trim().to_string()),
                },
            };
        }
        if let Some(target) = path {
            // untracked：从未提交，revert = 删除工作区文件
            if self.is_untracked(cwd, target) {
                return match std::fs::remove_file(std::path::Path::new(cwd).join(target)) {
                    Ok(_) => OpResult {
                        ok: true,
                        message: None,
                    },
                    Err(e) => OpResult {
                        ok: false,
                        message: Some(format!("删除 untracked 文件失败: {e}")),
                    },
                };
            }
            return match run(cwd, &["restore", "--staged", "--worktree", "--", target]) {
                Ok(_) => OpResult {
                    ok: true,
                    message: None,
                },
                Err(e) => OpResult {
                    ok: false,
                    message: Some(e.stderr.trim().to_string()),
                },
            };
        }
        // 全部变更：restore tracked + clean untracked
        if let Err(e) = run(cwd, &["restore", "--staged", "--worktree", "--", "."]) {
            return OpResult {
                ok: false,
                message: Some(e.stderr.trim().to_string()),
            };
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
            .and_then(|t| t.lookup_entry_by_path(gix::path::from_bstr(&full)).ok().flatten())
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
    fn non_repo_marks_not_repo() {
        let dir = unique_dir("amux-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let st = GitRunner::new().diff(dir.to_str().unwrap(), None).not_repo;
        assert!(st);
    }

    #[test]
    fn diff_returns_structured_files() {
        let dir = init_repo();
        // 原 a.txt 两行 → 三行（删 line2，增 CHANGED 与 line3）
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
        // hunk patch 含文件头，可独立反向应用（单 hunk revert）
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
        // 已无变更时 restore 是幂等成功（no-op）
        let res = r.restore(dir.to_str().unwrap(), Some("a.txt"), None);
        assert!(res.ok, "幂等 revert 应成功: {:?}", res.message);
    }

    #[test]
    fn revert_single_hunk_via_patch() {
        // 独立仓库：20 行基线文件已提交；改第 3 行与第 18 行 → 两个独立 hunk
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
}
