//! 统一 diff 行的行号推算：`@@ -a,b +c,d @@` 给出起点，之后按行类别递增。
//!
//! daemon 侧的 hunk 行只带类别与内容，行号在此按 hunk 头还原（删除行不占新
//! 文件行号，新增行不占旧文件行号，上下文行两侧同时递增）。

use amux_common::domain::{GitDiffLine, GitDiffLineKind};

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
