//! git 直连运行器（docs/DESIGN.md §6）：只读 status/diff、写操作 push/revert。
//! cwd 非 git 仓库时 status 返回 `not_repo` 标记（GUI 不提供 diff 按钮）。

use std::process::Command;

use protocol::{GitChange, GitChangeStatus, GitOpResult, GitStatusResult};

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

    pub fn diff(&self, cwd: &str, path: Option<&str>) -> String {
        let mut args = vec!["diff", "HEAD"];
        if let Some(p) = path {
            args.push("--");
            args.push(p);
        }
        run(cwd, &args).unwrap_or_default()
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
    pub fn revert(&self, cwd: &str, path: Option<&str>, patch: Option<&str>) -> GitOpResult {
        if let Some(p) = patch {
            // 唯一临时目录（并发 revert 不互相覆盖）
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir =
                std::env::temp_dir().join(format!("amux-revert-{}-{nanos}", std::process::id()));
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
        let target = path.unwrap_or(".");
        match run(cwd, &["restore", "--staged", "--worktree", "--", target]) {
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
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
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
    }

    #[test]
    fn diff_returns_patch() {
        let dir = init_repo();
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED\n").unwrap();
        let d = GitRunner::new().diff(dir.to_str().unwrap(), None);
        assert!(d.contains("a.txt"));
    }
}
