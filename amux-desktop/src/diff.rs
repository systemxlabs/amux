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

/// 引用到会话输入框的一段内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// 改动所属文件路径
    pub path: String,
    /// 代码块内容（hunk 头 + 各行前缀与内容）；文件引用为 `None`
    pub hunk: Option<String>,
}

impl Reference {
    /// 插入会话输入框的文本：文件引用为文件路径，代码块引用为代码块内容。
    pub fn text(&self) -> String {
        match &self.hunk {
            Some(hunk) => format!("{}\n```diff\n{hunk}```", self.path),
            None => self.path.clone(),
        }
    }
}

/// 把改动审查中的选中项折算为引用内容，顺序与改动列表一致（文件在其代码块之前）。
///
/// 文件引用与代码块引用的内容不同（路径 / 代码块内容），两者不互相排斥。
pub fn selected_references(
    files: &[GitDiffFile],
    selected_files: &HashSet<String>,
    selected_hunks: &HashSet<(String, String)>,
) -> Vec<Reference> {
    let mut references = Vec::new();
    for file in files {
        if selected_files.contains(&file.path) {
            references.push(Reference {
                path: file.path.clone(),
                hunk: None,
            });
        }
        for hunk in &file.hunks {
            if selected_hunks.contains(&(file.path.clone(), hunk.header.clone())) {
                references.push(Reference {
                    path: file.path.clone(),
                    hunk: Some(hunk_content(hunk)),
                });
            }
        }
    }
    references
}

/// 代码块内容：hunk 头 + 各行（前缀 + 内容），与 inline 展示的形状一致。
fn hunk_content(hunk: &GitDiffHunk) -> String {
    let mut content = String::new();
    content.push_str(&hunk.header);
    content.push('\n');
    for line in &hunk.lines {
        content.push(line.kind.prefix());
        content.push_str(&line.text);
        content.push('\n');
    }
    content
}

#[cfg(test)]
mod tests {
    use amux_common::domain::GitChangeStatus;

    use super::*;

    fn line(kind: GitDiffLineKind, text: &str) -> GitDiffLine {
        GitDiffLine {
            kind,
            text: text.to_string(),
        }
    }

    fn hunk(header: &str, lines: Vec<GitDiffLine>) -> GitDiffHunk {
        GitDiffHunk {
            header: header.to_string(),
            lines,
        }
    }

    fn file(path: &str, hunks: Vec<GitDiffHunk>) -> GitDiffFile {
        GitDiffFile {
            path: path.to_string(),
            status: GitChangeStatus::Modified,
            additions: 1,
            deletions: 0,
            hunks,
        }
    }

    /// 文件引用为文件路径本身。
    #[test]
    fn file_reference_is_its_path() {
        let files = [file("src/a.rs", vec![hunk("@@ -1,2 +1,2 @@", Vec::new())])];
        let selected_files = HashSet::from(["src/a.rs".to_string()]);

        let references = selected_references(&files, &selected_files, &HashSet::new());

        assert_eq!(references.len(), 1);
        assert_eq!(references[0].text(), "src/a.rs");
    }

    /// 代码块引用为代码块内容：hunk 头 + 各行前缀与内容。
    #[test]
    fn hunk_reference_is_its_content() {
        let files = [file(
            "src/a.rs",
            vec![hunk(
                "@@ -12,3 +12,3 @@ fn main()",
                vec![
                    line(GitDiffLineKind::Context, "let a = 1;"),
                    line(GitDiffLineKind::Remove, "let b = 2;"),
                    line(GitDiffLineKind::Add, "let b = 3;"),
                ],
            )],
        )];
        let selected_hunks = HashSet::from([(
            "src/a.rs".to_string(),
            "@@ -12,3 +12,3 @@ fn main()".to_string(),
        )]);

        let references = selected_references(&files, &HashSet::new(), &selected_hunks);

        assert_eq!(
            references[0].text(),
            "src/a.rs\n```diff\n@@ -12,3 +12,3 @@ fn main()\n let a = 1;\n-let b = 2;\n+let b = 3;\n```"
        );
    }

    /// 同一文件的文件引用与代码块引用内容不同，同时选中时两者都引用；
    /// 顺序与改动列表一致（文件在其代码块之前）。
    #[test]
    fn file_and_its_hunks_are_both_referenced_in_diff_order() {
        let files = [
            file(
                "src/a.rs",
                vec![
                    hunk("@@ -1,1 +1,1 @@", vec![line(GitDiffLineKind::Add, "a1")]),
                    hunk("@@ -9,1 +9,1 @@", vec![line(GitDiffLineKind::Add, "a2")]),
                ],
            ),
            file("src/b.rs", vec![hunk("@@ -5,1 +5,1 @@", Vec::new())]),
        ];
        let selected_files = HashSet::from(["src/a.rs".to_string()]);
        let selected_hunks = HashSet::from([
            ("src/b.rs".to_string(), "@@ -5,1 +5,1 @@".to_string()),
            ("src/a.rs".to_string(), "@@ -9,1 +9,1 @@".to_string()),
        ]);

        let references = selected_references(&files, &selected_files, &selected_hunks);

        assert_eq!(
            references,
            [
                Reference {
                    path: "src/a.rs".to_string(),
                    hunk: None,
                },
                Reference {
                    path: "src/a.rs".to_string(),
                    hunk: Some("@@ -9,1 +9,1 @@\n+a2\n".to_string()),
                },
                Reference {
                    path: "src/b.rs".to_string(),
                    hunk: Some("@@ -5,1 +5,1 @@\n".to_string()),
                },
            ]
        );
    }
}
