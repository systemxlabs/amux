//! 统一 diff 行的行号推算：`@@ -a,b +c,d @@` 给出起点，之后按行类别递增。
//!
//! daemon 侧的 hunk 行只带类别与内容，行号在此按 hunk 头还原（删除行不占新
//! 文件行号，新增行不占旧文件行号，上下文行两侧同时递增）。
//!
//! 另含改动审查中的评论目标与拖动行范围。

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

/// 改动审查中的评论目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentTarget {
    /// 评论整个文件。
    File { path: String },
    /// 评论拖动选中的连续代码。
    Code {
        path: String,
        hunk_header: String,
        code: String,
    },
}

impl CommentTarget {
    pub fn path(&self) -> &str {
        match self {
            Self::File { path } | Self::Code { path, .. } => path,
        }
    }

    pub fn hunk_header(&self) -> Option<&str> {
        match self {
            Self::File { .. } => None,
            Self::Code { hunk_header, .. } => Some(hunk_header),
        }
    }

    /// 评论作为用户消息发送到会话。
    pub fn message(&self, comment: &str) -> String {
        match self {
            Self::File { path } => format!("{path} {comment}"),
            Self::Code { code, .. } => format!("```\n{code}\n```\n{comment}"),
        }
    }
}

/// 改动代码行的稳定引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRef {
    pub path: String,
    pub hunk_header: String,
    pub line: usize,
}

/// 同一 hunk 内拖动形成的连续行范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSelection {
    pub start: LineRef,
    pub end: LineRef,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_follow_documented_message_shapes() {
        let file = CommentTarget::File {
            path: "src/a.rs".into(),
        };
        assert_eq!(file.message("请补充测试"), "src/a.rs 请补充测试");

        let code = CommentTarget::Code {
            path: "src/a.rs".into(),
            hunk_header: "@@ -1,2 +1,2 @@".into(),
            code: "-let old = 1;\n+let new = 1;".into(),
        };
        assert_eq!(
            code.message("这里需要说明"),
            "```\n-let old = 1;\n+let new = 1;\n```\n这里需要说明"
        );
    }
}
