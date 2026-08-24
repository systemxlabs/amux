//! Diff 渲染纯函数（PRD「文件改动审查」）：hunk patch → 行级数据。
//! 与 GPUI 状态分离，便于单测。

use protocol::GitDiffHunk;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffLineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone)]
pub(crate) struct DiffLine {
    pub(crate) old_number: Option<usize>,
    pub(crate) new_number: Option<usize>,
    pub(crate) kind: DiffLineKind,
    pub(crate) content: String,
}

/// hunk 头中指定侧的起始行号（缺省按 1 处理）。
fn hunk_start(header: &str, prefix: char) -> usize {
    header
        .split_whitespace()
        .find_map(|part| part.strip_prefix(prefix))
        .and_then(|range| range.split(',').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or(1)
}

/// 完整 hunk patch → 行级渲染数据（行号 + 类别 + 内容）。
pub(crate) fn diff_lines(hunk: &GitDiffHunk) -> Vec<DiffLine> {
    let mut in_body = false;
    let mut old_number = hunk_start(&hunk.header, '-');
    let mut new_number = hunk_start(&hunk.header, '+');
    let mut lines = Vec::new();

    for line in hunk.patch.lines() {
        if !in_body {
            in_body = line.starts_with("@@ ");
            continue;
        }
        let Some(prefix) = line.chars().next() else {
            continue;
        };
        let content = &line[prefix.len_utf8()..];
        // 仅 '+ '/'-'/' ' 三类；'\'（无换行符标记）等其余前缀跳过
        let (kind, old, new) = match prefix {
            '+' => {
                let number = new_number;
                new_number += 1;
                (DiffLineKind::Addition, None, Some(number))
            }
            '-' => {
                let number = old_number;
                old_number += 1;
                (DiffLineKind::Deletion, Some(number), None)
            }
            ' ' => {
                let (o, n) = (old_number, new_number);
                old_number += 1;
                new_number += 1;
                (DiffLineKind::Context, Some(o), Some(n))
            }
            _ => continue,
        };
        lines.push(DiffLine {
            old_number: old,
            new_number: new,
            kind,
            content: content.to_string(),
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::GitDiffHunk;

    #[test]
    fn diff_lines_tracks_numbers_and_kinds() {
        let hunk = GitDiffHunk {
            header: "@@ -2,2 +2,3 @@".into(),
            patch: "diff --git a/x b/x\n@@ -2,2 +2,3 @@\n context\n-old\n+new1\n+new2\n".into(),
        };
        let lines = diff_lines(&hunk);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].kind, DiffLineKind::Context);
        assert_eq!(lines[0].old_number, Some(2));
        assert_eq!(lines[0].new_number, Some(2));
        assert_eq!(lines[1].kind, DiffLineKind::Deletion);
        assert_eq!(lines[1].old_number, Some(3));
        assert_eq!(lines[1].new_number, None);
        assert_eq!(lines[2].kind, DiffLineKind::Addition);
        assert_eq!(lines[2].old_number, None);
        // context 之后 new 已推进到 3
        assert_eq!(lines[2].new_number, Some(3));
        assert_eq!(lines[3].new_number, Some(4));
    }

    /// 单侧省略 `,1` 的 hunk 头同样可解析。
    #[test]
    fn hunk_start_handles_omitted_count() {
        assert_eq!(hunk_start("@@ -5 +5,2 @@", '-'), 5);
        assert_eq!(hunk_start("@@ -5 +5,2 @@", '+'), 5);
        assert_eq!(hunk_start("@@ -0,0 +1 @@", '+'), 1);
    }
}
