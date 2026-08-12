//! GUI 纯逻辑（PRD §4.2 输入 / §3.1 会话排序 / §4.1.1 会话列表惰性加载）：
//! @ 引用解析、附件 → prompt 组装、最近活跃排序键、会话列表窗口合并。
//! 与 GPUI 渲染分离，可单测直驱。

use protocol::{ContentBlock, SessionMeta};

/// 输入附件：@ 引用文件/目录、拖拽文件、粘贴图片、语音。
#[derive(Debug, Clone, PartialEq)]
pub enum InputAttachment {
    /// @ 引用或拖拽的路径（文件或目录）
    Path { path: String, is_dir: bool },
    /// 粘贴的图片（base64 数据）
    Image {
        name: String,
        mime_type: String,
        data_base64: String,
    },
    /// 语音录音（base64 数据）
    Audio {
        name: String,
        mime_type: String,
        data_base64: String,
    },
}

/// 路径 → 路径附件（PRD §4.2）。拖拽文件/目录与 `@` 引用共用：
/// 两者产生的都是 `InputAttachment::Path`，经同一 `compose_prompt` 管线组装。
pub fn path_attachment(path: &str) -> InputAttachment {
    InputAttachment::Path {
        path: path.to_string(),
        is_dir: std::path::Path::new(path).is_dir(),
    }
}

/// 解析输入文本中的 @ 引用（PRD §4.2：@ 引用文件或目录作为上下文）。
/// 返回（清理后的文本，引用列表）。@ 后跟路径直到空白/行尾。
pub fn parse_at_references(text: &str) -> (String, Vec<String>) {
    let mut refs = Vec::new();
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find('@') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        // 路径：直到空白字符
        let end = after
            .find(|c: char| c.is_whitespace() || c == '@')
            .unwrap_or(after.len());
        let path = after[..end].trim();
        if path.is_empty() {
            out.push('@');
            rest = after;
            continue;
        }
        // 邮箱（@ 前有字母数字）与纯标点不当作引用
        let prev_is_word = out
            .chars()
            .last()
            .map(|c| c.is_alphanumeric())
            .unwrap_or(false);
        if prev_is_word {
            out.push('@');
            rest = after;
            continue;
        }
        refs.push(path.to_string());
        rest = &after[end..];
    }
    out.push_str(rest);
    (out, refs)
}

/// 读取 @ 引用路径为上下文文本（文件读内容；目录列条目；不存在则标注）。
pub fn read_path_context(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let mut entries: Vec<String> = std::fs::read_dir(p)
            .map(|it| {
                it.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        format!("[目录 {path}] {}", entries.join(", "))
    } else if p.is_file() {
        let content = std::fs::read_to_string(p).unwrap_or_default();
        let excerpt: String = content.chars().take(4000).collect();
        format!("[文件 {path}]\n{excerpt}")
    } else {
        format!("[引用不存在 {path}]")
    }
}

/// 附件 → prompt 内容块（PRD §4.2 上下文）。
pub fn attachment_to_content_block(a: &InputAttachment) -> ContentBlock {
    match a {
        InputAttachment::Path { path, .. } => ContentBlock::Text {
            text: format!("[上下文附件] {}", read_path_context(path)),
        },
        InputAttachment::Image {
            name,
            mime_type,
            data_base64,
        } => ContentBlock::Resource {
            mime_type: mime_type.clone(),
            uri: Some(format!("data:image;name={name}")),
            text: None,
            blob: Some(data_base64.clone()),
        },
        InputAttachment::Audio {
            name,
            mime_type,
            data_base64,
        } => ContentBlock::Resource {
            mime_type: mime_type.clone(),
            uri: Some(format!("data:audio;name={name}")),
            text: None,
            blob: Some(data_base64.clone()),
        },
    }
}

/// 输入文本 + 附件 → prompt 内容块列表。
pub fn compose_prompt(text: &str, attachments: &[InputAttachment]) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !text.trim().is_empty() {
        blocks.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    for a in attachments {
        blocks.push(attachment_to_content_block(a));
    }
    blocks
}

/// 把一窗会话并入已加载列表（PRD §4.1.1 惰性加载：首次取最近活跃一窗，滚动加载更早）。
/// - 首次加载：`existing` 为空 → 窗口即为当前列表
/// - 追加：更早一窗按序并入（保持最近活跃在前）
/// - 按 id 去重（并发通知 / 游标边界可能重复）
/// 返回（合并后的列表, 是否还有更早, 下次 before 游标）。
pub fn merge_session_window(
    existing: &[SessionMeta],
    window: Vec<SessionMeta>,
    has_more: bool,
    next_before: Option<u64>,
) -> (Vec<SessionMeta>, bool, Option<u64>) {
    let mut out = existing.to_vec();
    for m in window {
        if !out.iter().any(|s| s.id == m.id) {
            out.push(m);
        }
    }
    (out, has_more, next_before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_at_references_extracts_paths_and_cleans_text() {
        let (text, refs) = parse_at_references("用 @src/main.rs 的代码实现功能");
        assert_eq!(text, "用  的代码实现功能");
        assert_eq!(refs, vec!["src/main.rs"]);

        // 多个引用
        let (text, refs) = parse_at_references("实现 @a/b 和 @c/d 两个文件");
        assert_eq!(text, "实现  和  两个文件");
        assert_eq!(refs, vec!["a/b", "c/d"]);

        // 行尾引用
        let (text, refs) = parse_at_references("查看 @README.md");
        assert_eq!(text, "查看 ");
        assert_eq!(refs, vec!["README.md"]);

        // 邮箱（@ 前是字母）不解析；末尾的 @ 后无路径也不解析
        let (text, refs) = parse_at_references("联系 a@b.com 或 @ ");
        assert_eq!(refs.len(), 0);
        assert_eq!(text, "联系 a@b.com 或 @ ");
    }

    #[test]
    fn attachment_to_content_block_maps_kinds() {
        // 图片附件 → Resource 块（blob 携带数据）
        let img = InputAttachment::Image {
            name: "x.png".into(),
            mime_type: "image/png".into(),
            data_base64: "AAAA".into(),
        };
        match attachment_to_content_block(&img) {
            ContentBlock::Resource {
                mime_type,
                blob,
                uri,
                ..
            } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(blob.as_deref(), Some("AAAA"));
                assert!(uri.as_deref().unwrap().contains("x.png"));
            }
            _ => panic!("图片附件应映射为 Resource 块"),
        }
        // 音频附件 → Resource 块
        let audio = InputAttachment::Audio {
            name: "v.webm".into(),
            mime_type: "audio/webm".into(),
            data_base64: String::new(),
        };
        assert!(matches!(
            attachment_to_content_block(&audio),
            ContentBlock::Resource { mime_type, .. } if mime_type == "audio/webm"
        ));
        // 路径附件 → 文本块（含上下文）
        let path = InputAttachment::Path {
            path: "不存在的路径".into(),
            is_dir: false,
        };
        assert!(matches!(
            attachment_to_content_block(&path),
            ContentBlock::Text { .. }
        ));
    }

    #[test]
    fn compose_prompt_builds_blocks() {
        let blocks = compose_prompt(
            "你好",
            &[InputAttachment::Path {
                path: "nonexistent.txt".into(),
                is_dir: false,
            }],
        );
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "你好"));
        // 空文本 + 附件
        let blocks = compose_prompt(
            "  ",
            &[InputAttachment::Path {
                path: "x".into(),
                is_dir: false,
            }],
        );
        assert_eq!(blocks.len(), 1);
    }

    /// 拖拽路径与 @ 引用路径走**同一管线**（PRD §4.2）：拖入 → `path_attachment` →    /// `compose_prompt` → 文本上下文块；@ 引用解析后同样转为 `path_attachment` →
    /// `compose_prompt`。二者产出的**附件内容块一致**。
    #[test]
    fn dropped_path_and_at_reference_share_pipeline() {
        // 准备一个真实文件与目录（附件读取真实内容）
        let dir = std::env::temp_dir().join(format!("amux-drop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "拖拽内容").unwrap();

        // 拖入：drop 处理器产生 Path 附件 → compose_prompt
        let dropped = path_attachment(&file.display().to_string());
        assert!(matches!(dropped, InputAttachment::Path { is_dir: false, .. }));
        let drop_blocks = compose_prompt("", &[dropped.clone()]);

        // @ 引用：解析出同一路径 → 同样的 Path 附件 → 同一 compose_prompt
        let text = format!("看 @{}", file.display());
        let (clean, refs) = parse_at_references(&text);
        assert_eq!(refs, vec![file.display().to_string()]);
        assert!(!clean.trim().is_empty(), "@ 引用应把路径从文本中剥离");
        let all: Vec<InputAttachment> = refs.iter().map(|r| path_attachment(r)).collect();
        let at_blocks = compose_prompt("", &all);

        // 附件内容块完全一致（同一管线产出同一块）
        assert_eq!(drop_blocks, at_blocks);
        assert_eq!(drop_blocks.len(), 1);
        let ContentBlock::Text { text } = &drop_blocks[0] else {
            panic!("路径附件应产生文本上下文块");
        };
        assert!(text.contains("拖拽内容"), "附件应读取文件内容: {text}");

        // 与文本混排时（真实发送路径）：文本块在前、附件块在后，附件块与纯拖入一致
        let with_text = compose_prompt(&clean, &all);
        assert_eq!(with_text.len(), 2);
        assert_eq!(with_text[1], drop_blocks[0], "文本后的附件块应一致");

        // 拖入目录 → is_dir=true（目录条目列表作为上下文）
        let dir_att = path_attachment(&dir.display().to_string());
        assert!(matches!(dir_att, InputAttachment::Path { is_dir: true, .. }));
        let dir_blocks = compose_prompt("", &[dir_att]);
        let ContentBlock::Text { text } = &dir_blocks[0] else {
            panic!("目录附件应产生文本上下文块");
        };
        assert!(text.contains("notes.txt"), "目录附件应列出条目: {text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn smeta(id: &str, last: u64) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            harness: "codex".into(),
            cwd: "/tmp".into(),
            model: None,
            state: protocol::SessionState::Idle,
            interrupted: false,
            title: String::new(),
            created_at: 1,
            last_event_at: last,
        }
    }

    /// 会话列表惰性加载（PRD §4.1.1）：首次窗口 + 滚动追加 + 按 id 去重。
    #[test]
    fn merge_session_window_first_and_append() {
        // 首次加载（空列表 → 一窗）
        let window1 = vec![smeta("s3", 300), smeta("s2", 200)];
        let (list, has_more, next) = merge_session_window(&[], window1, true, Some(200));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "s3");
        assert_eq!(list[1].id, "s2");
        assert!(has_more);
        assert_eq!(next, Some(200));

        // 滚动加载更早一窗：追加（保持最近活跃在前）
        let window2 = vec![smeta("s1", 100)];
        let (list, has_more, next) = merge_session_window(&list, window2, false, None);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1"],
            "更早一窗应追加在已加载之后（最近活跃在前）"
        );
        assert!(!has_more);
        assert_eq!(next, None);

        // 重复 id 去重（并发通知/游标边界可能重复）
        let window3 = vec![smeta("s2", 200), smeta("s1", 100), smeta("s0", 50)];
        let (list, _, _) = merge_session_window(&list, window3, false, None);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1", "s0"],
            "重复 id 不应重复出现"
        );
    }
}
