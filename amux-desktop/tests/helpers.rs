//! 纯逻辑助手（文本/时间/气泡宽度/diff 行号）的集成测试。
//!
//! 放在 `tests/`：库内联测试会触发组件宏的深度展开，编译期爆栈。

use amux_common::domain::{GitDiffLine, GitDiffLineKind};
use amux_desktop::{diff, ui};
use gpui::px;

#[test]
fn context_usage_text_hides_missing_and_formats_usage() {
    assert_eq!(ui::context_usage_text(0, 0), None);
    assert_eq!(ui::context_usage_text(53000, 0), Some("53,000 token".into()));
    assert_eq!(
        ui::context_usage_text(53000, 200000),
        Some("53,000 / 200,000 token（26.5%）".into())
    );
}

#[test]
fn one_line_collapses_whitespace() {
    assert_eq!(ui::one_line("a\n b\t c"), "a b c");
}

#[test]
fn short_cwd_handles_platform_separators() {
    assert_eq!(ui::short_cwd("/home/user/project"), "project");
    assert_eq!(ui::short_cwd("/"), "/");
    assert_eq!(ui::short_cwd(""), "");
}

#[test]
fn bubble_width_scales_with_content_and_clamps() {
    let narrow = ui::estimate_bubble_width("hi", 14.0, 132.0, 720.0);
    let wide = ui::estimate_bubble_width(&"x".repeat(400), 14.0, 132.0, 720.0);
    let empty = ui::estimate_bubble_width("", 14.0, 132.0, 720.0);
    assert_eq!(empty, px(132.0));
    assert!(narrow < wide);
    assert_eq!(wide, px(720.0));
}

fn line(kind: GitDiffLineKind, text: &str) -> GitDiffLine {
    GitDiffLine {
        kind,
        text: text.to_string(),
    }
}

#[test]
fn diff_numbers_start_at_hunk_header_and_advance_per_side() {
    let lines = vec![
        line(GitDiffLineKind::Remove, "old a"),
        line(GitDiffLineKind::Add, "new a"),
        line(GitDiffLineKind::Add, "new b"),
        line(GitDiffLineKind::Context, "shared"),
        line(GitDiffLineKind::Remove, "old b"),
    ];
    let numbers = diff::line_numbers("@@ -10,3 +20,4 @@ fn main()", &lines);
    assert_eq!(
        numbers,
        vec![
            diff::LineNumbers {
                old: Some(10),
                new: None
            },
            diff::LineNumbers {
                old: None,
                new: Some(20)
            },
            diff::LineNumbers {
                old: None,
                new: Some(21)
            },
            diff::LineNumbers {
                old: Some(11),
                new: Some(22)
            },
            diff::LineNumbers {
                old: Some(12),
                new: None
            },
        ]
    );
}

#[test]
fn diff_numbers_fall_back_to_first_line_for_unparsable_header() {
    let numbers = diff::line_numbers("@@", &[line(GitDiffLineKind::Context, "x")]);
    assert_eq!(
        numbers,
        vec![diff::LineNumbers {
            old: Some(1),
            new: Some(1)
        }]
    );
}
