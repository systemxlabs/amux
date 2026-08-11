//! git 直连运行器（docs/DESIGN.md §6）：只读 status/diff、写操作 push/revert。
//! cwd 非 git 仓库时 status 返回 `not_repo` 标记（GUI 不提供 diff 按钮）。

use std::process::Command;

use protocol::{
    GitChange, GitChangeStatus, GitDiffFile, GitDiffHunk, GitDiffResult, GitOpResult,
    GitStatusResult,
};

pub struct GitRunner;

/// git 输出错误（stdout/stderr 用于诊断）。
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

/// 是否"不是 git 仓库"类错误（含中英文 git 输出）。
fn is_not_repo(e: &GitError) -> bool {
    let msg = format!("{}\n{}", e.message, e.stderr);
    msg.contains("not a git repository") || msg.contains("不是 Git 仓库")
}

/// 解析 `git diff HEAD` 输出为按文件的 GitDiffFile（含 hunk 拆分与增减统计）。
/// 纯函数，可单测。
pub fn parse_diff(raw: &str) -> Vec<GitDiffFile> {
    let mut files: Vec<GitDiffFile> = Vec::new();
    let mut cur: Option<GitDiffFile> = None;
    let mut file_header: String = String::new(); // 首个 hunk 前的文件头（diff --git/index/---/+++）
    let mut in_hunk = false;

    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            // 结束上一个文件
            if let Some(f) = cur.take() {
                files.push(f);
            }
            // 路径：`diff --git a/x b/y`——重命名时 x≠y，取 b/ 侧
            let path = line
                .split_whitespace()
                .nth(3)
                .and_then(|p| p.strip_prefix("b/"))
                .unwrap_or("")
                .to_string();
            cur = Some(GitDiffFile {
                path,
                status: GitChangeStatus::Modified,
                additions: 0,
                deletions: 0,
                patch: String::new(),
                hunks: Vec::new(),
            });
            file_header.clear();
            in_hunk = false;
            file_header.push_str(line);
            file_header.push('\n');
            if let Some(f) = cur.as_mut() {
                f.patch.push_str(line);
                f.patch.push('\n');
            }
            continue;
        }
        let Some(f) = cur.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode") {
            f.status = GitChangeStatus::Added;
        } else if line.starts_with("deleted file mode") {
            f.status = GitChangeStatus::Deleted;
        } else if line.starts_with("similarity index") {
            f.status = GitChangeStatus::Renamed;
        } else if line.starts_with("rename to ") {
            f.path = line
                .strip_prefix("rename to ")
                .map(str::trim)
                .unwrap_or("")
                .to_string();
        } else if line.starts_with("+++ b/") && f.path.is_empty() {
            f.path = line.strip_prefix("+++ b/").unwrap_or("").trim().to_string();
        }

        if line.starts_with("@@ ") {
            in_hunk = true;
            // hunk patch = 文件头 + 该 hunk（可独立 `git apply --reverse`，PRD §3.5 单 hunk revert）
            let mut hp = file_header.clone();
            hp.push_str(line);
            hp.push('\n');
            f.hunks.push(GitDiffHunk {
                header: line.to_string(),
                patch: hp,
            });
            f.patch.push_str(line);
            f.patch.push('\n');
            continue;
        }

        if in_hunk {
            if line.starts_with('+') && !line.starts_with("+++") {
                f.additions += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                f.deletions += 1;
            }
            if let Some(h) = f.hunks.last_mut() {
                h.patch.push_str(line);
                h.patch.push('\n');
            }
            f.patch.push_str(line);
            f.patch.push('\n');
        } else {
            file_header.push_str(line);
            file_header.push('\n');
            f.patch.push_str(line);
            f.patch.push('\n');
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

impl GitRunner {
    pub fn new() -> Self {
        GitRunner
    }

    pub fn status(&self, cwd: &str) -> Result<GitStatusResult, GitError> {
        let branch = match run(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            Ok(b) => b.trim().to_string(),
            Err(e) if is_not_repo(&e) => {
                return Ok(GitStatusResult {
                    branch: String::new(),
                    changes: Vec::new(),
                    not_repo: true,
                })
            }
            Err(e) => return Err(e),
        };
        let porcelain = run(cwd, &["status", "--porcelain=v1"])?;
        let numstat = self.numstat(cwd);
        let mut changes = Vec::new();
        for line in porcelain.lines() {
            if line.is_empty() {
                continue;
            }
            let xy = &line[..2];
            let rest = line[3..].to_string();
            if xy == "??" {
                changes.push(GitChange {
                    path: rest,
                    status: GitChangeStatus::Untracked,
                    staged: false,
                    additions: 0,
                    deletions: 0,
                });
                continue;
            }
            let x = xy.chars().nth(0).unwrap_or(' ');
            let y = xy.chars().nth(1).unwrap_or(' ');
            let path = rest
                .rsplit(" -> ")
                .next()
                .unwrap_or(rest.as_str())
                .to_string();
            let status = if x == 'A' || y == 'A' {
                GitChangeStatus::Added
            } else if x == 'D' || y == 'D' {
                GitChangeStatus::Deleted
            } else if x == 'R' || y == 'R' {
                GitChangeStatus::Renamed
            } else {
                GitChangeStatus::Modified
            };
            let staged = x != ' ' && x != '?';
            let (adds, dels) = numstat.get(&path).copied().unwrap_or((0, 0));
            changes.push(GitChange {
                path,
                status,
                staged,
                additions: adds,
                deletions: dels,
            });
        }
        Ok(GitStatusResult {
            branch,
            changes,
            not_repo: false,
        })
    }

    fn numstat(&self, cwd: &str) -> std::collections::HashMap<String, (u32, u32)> {
        let mut map = std::collections::HashMap::new();
        let Ok(stdout) = run(cwd, &["diff", "HEAD", "--numstat"]) else {
            return map;
        };
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let adds = parts[0].parse().unwrap_or(0);
            let dels = parts[1].parse().unwrap_or(0);
            map.insert(parts[2..].join("\t"), (adds, dels));
        }
        map
    }

    /// 结构化 diff（PRD §3.5）：按文件拆分，含每文件增减行数、完整 patch 与 hunk 列表。
    /// cwd 非 git 仓库时返回 `not_repo` 标记。
    pub fn diff(&self, cwd: &str, path: Option<&str>) -> GitDiffResult {
        // 先确认是 git 仓库（非仓库时返回 not_repo，与 status 一致）
        if let Err(e) = run(cwd, &["rev-parse", "--git-dir"]) {
            if is_not_repo(&e) {
                return GitDiffResult {
                    files: Vec::new(),
                    not_repo: true,
                };
            }
            return GitDiffResult {
                files: Vec::new(),
                not_repo: false,
            };
        }
        let mut args = vec!["diff", "HEAD", "--no-color", "--unified=3"];
        if let Some(p) = path {
            args.push("--");
            args.push(p);
        }
        let Ok(raw) = run(cwd, &args) else {
            return GitDiffResult {
                files: Vec::new(),
                not_repo: false,
            };
        };
        GitDiffResult {
            files: parse_diff(&raw),
            not_repo: false,
        }
    }

    pub fn push(&self, cwd: &str) -> GitOpResult {
        match run(cwd, &["push", "origin", "HEAD"]) {
            Ok(_) => GitOpResult {
                ok: true,
                message: None,
            },
            Err(e) => GitOpResult {
                ok: false,
                message: Some(e.stderr.trim().to_string()),
            },
        }
    }

    /// 撤销工作区变更（undo 语义；调用方须保证会话空闲，docs/DESIGN.md §6）。
    /// - `patch`：单 hunk/单文件 patch 反向应用（`git apply --reverse`）
    /// - `path`：单文件——tracked 用 restore；untracked 直接删除（从未提交，revert = 移除）
    /// - 都不给：全部变更——restore 全部 tracked 变更 + clean 全部 untracked
    pub fn revert(&self, cwd: &str, path: Option<&str>, patch: Option<&str>) -> GitOpResult {
        if let Some(p) = patch {
            // 唯一临时目录（并发 revert 不互相覆盖；uuid v4）
            let dir = std::env::temp_dir().join(format!("amux-revert-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).ok();
            let patch_file = dir.join("revert.patch");
            if std::fs::write(&patch_file, p).is_err() {
                return GitOpResult {
                    ok: false,
                    message: Some("写入 patch 失败".into()),
                };
            }
            return match run(cwd, &["apply", "--reverse", &patch_file.to_string_lossy()]) {
                Ok(_) => GitOpResult {
                    ok: true,
                    message: None,
                },
                Err(e) => GitOpResult {
                    ok: false,
                    message: Some(e.stderr.trim().to_string()),
                },
            };
        }
        if let Some(target) = path {
            // untracked：从未提交，revert = 删除工作区文件
            if self.is_untracked(cwd, target) {
                return match std::fs::remove_file(std::path::Path::new(cwd).join(target)) {
                    Ok(_) => GitOpResult {
                        ok: true,
                        message: None,
                    },
                    Err(e) => GitOpResult {
                        ok: false,
                        message: Some(format!("删除 untracked 文件失败: {e}")),
                    },
                };
            }
            return match run(cwd, &["restore", "--staged", "--worktree", "--", target]) {
                Ok(_) => GitOpResult {
                    ok: true,
                    message: None,
                },
                Err(e) => GitOpResult {
                    ok: false,
                    message: Some(e.stderr.trim().to_string()),
                },
            };
        }
        // 全部变更：restore tracked + clean untracked
        if let Err(e) = run(cwd, &["restore", "--staged", "--worktree", "--", "."]) {
            return GitOpResult {
                ok: false,
                message: Some(e.stderr.trim().to_string()),
            };
        }
        match run(cwd, &["clean", "-fd"]) {
            Ok(_) => GitOpResult {
                ok: true,
                message: None,
            },
            Err(e) => GitOpResult {
                ok: false,
                message: Some(e.stderr.trim().to_string()),
            },
        }
    }

    /// 目标路径是否 untracked（porcelain 输出 `?? ` 前缀）。
    fn is_untracked(&self, cwd: &str, target: &str) -> bool {
        let Ok(out) = run(cwd, &["status", "--porcelain=v1", "--", target]) else {
            return false;
        };
        out.lines()
            .any(|l| l.starts_with("?? ") || l.starts_with("??"))
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
    fn status_lists_changes() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let r = GitRunner::new();
        let st = r.status(dir.to_str().unwrap()).unwrap();
        assert!(!st.not_repo);
        assert_eq!(st.branch, "main");
        assert!(st
            .changes
            .iter()
            .any(|c| c.path == "a.txt" && matches!(c.status, GitChangeStatus::Modified)));
        assert!(st
            .changes
            .iter()
            .any(|c| c.path == "new.txt" && matches!(c.status, GitChangeStatus::Untracked)));
    }

    #[test]
    fn non_repo_marks_not_repo() {
        let dir = unique_dir("amux-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let st = GitRunner::new().status(dir.to_str().unwrap()).unwrap();
        assert!(st.not_repo);
        // git_diff 同样返回 not_repo（GUI 不提供 diff 按钮）
        let d = GitRunner::new().diff(dir.to_str().unwrap(), None);
        assert!(d.not_repo);
        assert!(d.files.is_empty());
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
        let res = r.revert(dir.to_str().unwrap(), Some("a.txt"), None);
        assert!(res.ok, "revert 失败: {:?}", res.message);
        let content = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert_eq!(content, "line1\nline2\n", "工作区应恢复到 HEAD");
        // 已无变更时 restore 是幂等成功（no-op）
        let res = r.revert(dir.to_str().unwrap(), Some("a.txt"), None);
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
        let res = GitRunner::new().revert(dir.to_str().unwrap(), None, Some(&hunk.patch));
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
        let res = GitRunner::new().revert(dir.to_str().unwrap(), Some("scratch.txt"), None);
        assert!(res.ok, "untracked revert 失败: {:?}", res.message);
        assert!(!dir.join("scratch.txt").exists(), "untracked 应被删除");
    }

    #[test]
    fn parse_diff_splits_multiple_files() {
        let raw = "\
diff --git a/a.txt b/a.txt
index 111..222 100644
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,4 @@
 line1
-old
+new
 line3
diff --git a/b.txt b/b.txt
new file mode 100644
index 000..333
--- /dev/null
+++ b/b.txt
@@ -0,0 +1,2 @@
+x
+y
";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].additions, 1);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[1].path, "b.txt");
        assert!(matches!(files[1].status, GitChangeStatus::Added));
        assert_eq!(files[1].additions, 2);
    }
}
