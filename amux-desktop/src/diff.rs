//! 统一 diff 行的行号推算：`@@ -a,b +c,d @@` 给出起点，之后按行类别递增。
//!
//! daemon 侧的 hunk 行只带类别与内容，行号在此按 hunk 头还原（删除行不占新
//! 文件行号，新增行不占旧文件行号，上下文行两侧同时递增）。
//!
//! 另含改动审查中选中改动的折算（引用文件或代码块到会话输入框）。

use std::collections::HashSet;

use amux_common::domain::{GitDiffFile, GitDiffHunk, GitDiffLine, GitDiffLineKind};

/// hunk 中一行的旧/新文件行号（缺省表示该侧无此行号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineNumbers {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// 解析 `@@ -a,b +c,d @@` 的起始行号；无法解析时返回 `(1, 1)`。
fn hunk_start(header: &str) -> (usize, usize) {
    let mut parts = header.split_whitespace();
    let old = parts.nth(1).unwrap_or_default();
    let new = parts.next().unwrap_or_default();
    let parse = |part: &str, sign: char| -> usize {
        part.strip_prefix(sign)
            .and_then(|rest| rest.split(',').next())
            .and_then(|start| start.parse().ok())
            .unwrap_or(1)
    };
    (parse(old, '-'), parse(new, '+'))
}

/// 按 hunk 头与行类别推算每行的旧/新行号。
pub fn line_numbers(header: &str, lines: &[GitDiffLine]) -> Vec<LineNumbers> {
    let (mut old, mut new) = hunk_start(header);
    lines
        .iter()
        .map(|line| match line.kind {
            GitDiffLineKind::Context => {
                let numbers = LineNumbers {
                    old: Some(old),
                    new: Some(new),
                };
                old += 1;
                new += 1;
                numbers
            }
            GitDiffLineKind::Remove => {
                let numbers = LineNumbers {
                    old: Some(old),
                    new: None,
                };
                old += 1;
                numbers
            }
            GitDiffLineKind::Add => {
                let numbers = LineNumbers {
                    old: None,
                    new: Some(new),
                };
                new += 1;
                numbers
            }
        })
        .collect()
}

/// 引用到会话输入框的一段改动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// 输入区展示用的标签：整文件为路径，代码块为 `路径:起始行`
    pub label: String,
    /// 该段改动的完整 patch 文本
    pub patch: String,
}

/// 把改动审查中的选中项折算为引用片段，顺序与改动列表一致。
///
/// 整文件被选中时只引用该文件：其代码块不再单列，同一段改动不重复引用。
pub fn selected_references(
    files: &[GitDiffFile],
    selected_files: &HashSet<String>,
    selected_hunks: &HashSet<(String, String)>,
) -> Vec<Reference> {
    let mut references = Vec::new();
    for file in files {
        if selected_files.contains(&file.path) {
            references.push(Reference {
                label: file.path.clone(),
                patch: file.patch.clone(),
            });
            continue;
        }
        for hunk in &file.hunks {
            if selected_hunks.contains(&(file.path.clone(), hunk.header.clone())) {
                references.push(Reference {
                    label: hunk_label(&file.path, hunk),
                    patch: hunk.patch.clone(),
                });
            }
        }
    }
    references
}

/// 代码块引用标签：新文件起始行（纯删除的代码块回落到旧文件行号）；
/// 代码块无行内容时无从推算行号，回落到 hunk 头。
fn hunk_label(path: &str, hunk: &GitDiffHunk) -> String {
    let start = line_numbers(&hunk.header, &hunk.lines)
        .first()
        .and_then(|numbers| numbers.new.or(numbers.old));
    match start {
        Some(line) => format!("{path}:{line}"),
        None => format!("{path} {}", hunk.header),
    }
}

#[cfg(test)]
mod tests {
    use amux_common::domain::GitChangeStatus;

    use super::*;

    /// 带一行新增内容的代码块（起始行号可由 hunk 头推算）。
    fn hunk(header: &str) -> GitDiffHunk {
        GitDiffHunk {
            header: header.to_string(),
            patch: format!("patch {header}"),
            lines: vec![GitDiffLine {
                kind: GitDiffLineKind::Add,
                text: "added".to_string(),
            }],
        }
    }

    fn file(path: &str, hunks: Vec<GitDiffHunk>) -> GitDiffFile {
        GitDiffFile {
            path: path.to_string(),
            status: GitChangeStatus::Modified,
            additions: 1,
            deletions: 0,
            patch: format!("patch of {path}"),
            hunks,
        }
    }

    /// 整文件被选中时只引用该文件，其代码块不再单列。
    #[test]
    fn file_selection_suppresses_its_hunks() {
        let files = [file("src/a.rs", vec![hunk("@@ -1,2 +1,3 @@")])];
        let selected_files = HashSet::from(["src/a.rs".to_string()]);
        let selected_hunks =
            HashSet::from([("src/a.rs".to_string(), "@@ -1,2 +1,3 @@".to_string())]);

        assert_eq!(
            selected_references(&files, &selected_files, &selected_hunks),
            [Reference {
                label: "src/a.rs".to_string(),
                patch: "patch of src/a.rs".to_string(),
            }]
        );
    }

    /// 只选代码块时逐个引用，标签带路径与起始行，顺序与改动列表一致。
    #[test]
    fn hunk_selection_carries_hunk_patch() {
        let files = [
            file("src/a.rs", vec![hunk("@@ -1,2 +1,3 @@")]),
            file("src/b.rs", vec![hunk("@@ -9,2 +9,2 @@")]),
        ];
        let selected_hunks = HashSet::from([
            ("src/b.rs".to_string(), "@@ -9,2 +9,2 @@".to_string()),
            ("src/a.rs".to_string(), "@@ -1,2 +1,3 @@".to_string()),
        ]);

        assert_eq!(
            selected_references(&files, &HashSet::new(), &selected_hunks),
            [
                Reference {
                    label: "src/a.rs:1".to_string(),
                    patch: "patch @@ -1,2 +1,3 @@".to_string(),
                },
                Reference {
                    label: "src/b.rs:9".to_string(),
                    patch: "patch @@ -9,2 +9,2 @@".to_string(),
                },
            ]
        );
    }

    /// 无行内容的代码块无从推算行号，标签回落到 hunk 头。
    #[test]
    fn hunk_without_lines_labels_with_header() {
        let files = [file(
            "src/a.rs",
            vec![GitDiffHunk {
                header: "@@ -1 +1 @@".to_string(),
                patch: "patch".to_string(),
                lines: Vec::new(),
            }],
        )];
        let selected_hunks = HashSet::from([("src/a.rs".to_string(), "@@ -1 +1 @@".to_string())]);

        assert_eq!(
            selected_references(&files, &HashSet::new(), &selected_hunks),
            [Reference {
                label: "src/a.rs @@ -1 +1 @@".to_string(),
                patch: "patch".to_string(),
            }]
        );
    }
}
