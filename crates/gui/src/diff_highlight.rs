//! diff 语法高亮（docs/DESIGN.md §8.2：代码编辑器组件 + Tree Sitter 语法高亮）。
//! 纯逻辑与 GPUI 渲染分离：按文件扩展名推断语言、内容行/结构行判定、
//! Tree Sitter 高亮（gpui-component `SyntaxHighlighter`）、单行 StyledText 组装。
//! 与 server 侧 git 纯逻辑（parse_diff / hunk 拆分 / revert）无关，可独立单测。

use std::cell::RefCell;
use std::collections::HashMap;

use gpui::{HighlightStyle, StyledText};
use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
use ropey::Rope;

/// 按文件扩展名推断 diff 内容的代码语言。未知扩展返回空串（仅保留 +/- 语义着色，
/// 不做代码语言高亮）。
pub fn diff_language_for_path(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "py" => "python",
        "md" | "markdown" => "markdown",
        "json" | "jsonc" => "json",
        _ => "",
    }
}

/// diff 内容行的代码部分（去掉 `+`/`-`/空格 前缀）；结构行（diff --git / @@ / --- / +++
/// 等）返回 None，不参与代码高亮。
pub fn diff_content_code(line: &str) -> Option<&str> {
    let is_content = (line.starts_with('+') && !line.starts_with("+++"))
        || (line.starts_with('-') && !line.starts_with("---"))
        || line.starts_with(' ');
    if !is_content {
        return None;
    }
    Some(&line[1..])
}

/// 一个 patch 的内容行代码的 Tree Sitter 高亮结果。
/// - `spans`：与 `patch.lines()` 对齐——每行代码在解析文本中的字节区间（结构行为 None）
/// - `styles`：语法高亮样式（区间相对于解析文本，即内容行代码的拼接）
pub struct DiffPatchHighlight {
    spans: Vec<Option<(usize, usize)>>,
    styles: Vec<(std::ops::Range<usize>, HighlightStyle)>,
}

/// 对 patch 的内容行代码做 Tree Sitter 语法高亮（docs/DESIGN.md §8.2）：
/// 高亮器按语言缓存于渲染线程（thread_local，单线程访问无需加锁）。
/// `language` 为空串（未知扩展）时不做代码高亮（返回空样式）。
pub fn highlight_diff_patch(patch: &str, language: &str) -> DiffPatchHighlight {
    let mut text = String::new();
    let mut spans = Vec::new();
    for line in patch.lines() {
        match diff_content_code(line) {
            Some(code) => {
                let start = text.len();
                text.push_str(code);
                let end = text.len();
                text.push('\n');
                spans.push(Some((start, end)));
            }
            None => spans.push(None),
        }
    }
    let styles = if text.is_empty() || language.is_empty() {
        Vec::new()
    } else {
        highlight_code_text(&text, language)
    };
    DiffPatchHighlight { spans, styles }
}

/// Tree Sitter 高亮一段代码文本，返回 (字节区间, 样式)。
fn highlight_code_text(text: &str, language: &str) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    thread_local! {
        static HIGHLIGHTERS: RefCell<HashMap<String, SyntaxHighlighter>> =
            RefCell::new(HashMap::new());
    }
    HIGHLIGHTERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let hl = map
            .entry(language.to_string())
            .or_insert_with(|| SyntaxHighlighter::new(language));
        let rope = Rope::from_str(text);
        hl.update(None, &rope, None);
        hl.styles(&(0..text.len()), &HighlightTheme::default_light())
    })
}

/// 把偏移钳制到字符串的字符边界内（debug 构建 with_highlights 断言字符边界）。
fn clamp_char_boundary(text: &str, mut i: usize) -> usize {
    if i > text.len() {
        i = text.len();
    }
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 单行 diff → 语法高亮 StyledText：
/// - 内容行：`+`/`-`/空格 前缀字符保留，代码部分按文件语言着色（区间平移 1 字节）
/// - 结构行：无代码高亮（纯文本，保持原样）
pub fn diff_line_styled(line: &str, patch: &DiffPatchHighlight, line_index: usize) -> StyledText {
    let Some((s, e)) = patch.spans.get(line_index).copied().flatten() else {
        return StyledText::new(line.to_string());
    };
    if s >= e {
        return StyledText::new(line.to_string());
    }
    let line_styles: Vec<(std::ops::Range<usize>, HighlightStyle)> = patch
        .styles
        .iter()
        .filter(|(r, _)| r.end > s && r.start < e)
        .filter_map(|(r, st)| {
            let start = clamp_char_boundary(line, r.start.max(s) - s + 1);
            let end = clamp_char_boundary(line, r.end.min(e) - s + 1);
            if start >= end {
                return None;
            }
            Some((start..end, st.clone()))
        })
        .collect();
    StyledText::new(line.to_string()).with_highlights(line_styles)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语言推断：已知扩展 → 语言名；未知 → 空串（仅 +/- 语义着色）。
    #[test]
    fn diff_language_maps_extensions() {
        assert_eq!(diff_language_for_path("src/main.rs"), "rust");
        assert_eq!(diff_language_for_path("a/b.ts"), "typescript");
        assert_eq!(diff_language_for_path("x.js"), "javascript");
        assert_eq!(diff_language_for_path("x.py"), "python");
        assert_eq!(diff_language_for_path("README.md"), "markdown");
        assert_eq!(diff_language_for_path("Cargo.toml"), "");
        assert_eq!(diff_language_for_path("noext"), "");
        assert_eq!(diff_language_for_path("Makefile"), "");
    }

    /// 内容行/结构行判定：+/-/空格 前缀剥离；diff 元数据行不参与代码高亮。
    #[test]
    fn diff_content_code_splits_prefix() {
        assert_eq!(diff_content_code("+fn main() {}"), Some("fn main() {}"));
        assert_eq!(diff_content_code("-let x = 1;"), Some("let x = 1;"));
        assert_eq!(diff_content_code(" context"), Some("context"));
        assert_eq!(diff_content_code("@@ -1,2 +1,3 @@"), None);
        assert_eq!(diff_content_code("diff --git a/x b/x"), None);
        assert_eq!(diff_content_code("+++ b/x.rs"), None);
        assert_eq!(diff_content_code("--- a/x.rs"), None);
        assert_eq!(diff_content_code("index 111..222 100644"), None);
    }

    /// Tree Sitter 高亮走真实 shipped 管线（highlight_diff_patch → 解析 → styles）：
    /// 结构行不参与、内容行产生语法样式（rust 关键字 `fn` 获得主题色）。
    #[test]
    fn diff_highlight_produces_syntax_styles_for_rust() {
        let patch = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,2 +1,3 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
 }
";
        let hl = highlight_diff_patch(patch, "rust");
        // spans 与 patch.lines() 对齐
        assert_eq!(hl.spans.len(), patch.lines().count());
        // 结构行（diff --git / index / --- / +++ / @@）不参与代码高亮
        for i in 0..5 {
            assert!(hl.spans[i].is_none(), "第 {i} 行应为结构行");
        }
        // 内容行（上下文 / - / +）参与
        assert!(hl.spans[5].is_some());
        assert!(hl.spans[6].is_some());
        assert!(hl.spans[7].is_some());
        assert!(hl.spans[8].is_some());

        // `fn` 关键字（解析文本偏移 0..2）应被高亮为主题 keyword 色
        let keyword_color =
            HighlightTheme::default_light().style("keyword").and_then(|s| s.color);
        assert!(keyword_color.is_some(), "light 主题应定义 keyword 色");
        let has_fn = hl
            .styles
            .iter()
            .any(|(r, st)| r.start <= 0 && r.end >= 2 && st.color == keyword_color);
        assert!(has_fn, "`fn` 关键字应获得 keyword 语法高亮: {:?}", hl.styles);
    }

    /// 未知扩展/空 patch：不做代码高亮（空样式），渲染退化为纯文本。
    #[test]
    fn diff_highlight_unknown_language_falls_back_plain() {
        let patch = "diff --git a/x b/x\n@@ -1 +1 @@\n+x\n";
        let hl = highlight_diff_patch(patch, "");
        assert!(hl.styles.is_empty());
        // 结构行仍不参与
        assert!(hl.spans[0].is_none());
        assert!(hl.spans[1].is_none());
        assert!(hl.spans[2].is_some());

        let empty = highlight_diff_patch("", "rust");
        assert!(empty.styles.is_empty());
        assert!(empty.spans.is_empty());
    }

    /// diff_line_styled：内容行/结构行/越界行号都能构建（with_highlights 的
    /// 字符边界断言在 debug 构建下会 panic，构建成功即证明区间/偏移合法）。
    #[test]
    fn diff_line_styled_builds_for_content_and_header() {
        let patch = "diff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n fn main() {}\n";
        let hl = highlight_diff_patch(patch, "rust");
        // 结构行：无代码高亮，纯文本
        let _ = diff_line_styled("diff --git a/src/main.rs b/src/main.rs", &hl, 0);
        // 内容行：带高亮区间（含中文等非 ASCII 也应通过字符边界检查）
        let _ = diff_line_styled(" fn main() {}", &hl, 2);
        let _ = diff_line_styled("+println!(\"中文\");", &hl, 2);
        // 越界行号：spans 取不到 → 纯文本（不 panic）
        let _ = diff_line_styled(" fn main() {}", &hl, 99);
    }
}
