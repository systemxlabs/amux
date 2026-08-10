//! GUI 纯逻辑（PRD §4.2 输入 / §3.1 会话排序）：@ 引用解析、附件 → prompt 组装、
//! 最近活跃排序键。与 GPUI 渲染分离，可单测直驱。

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

/// 会话列表排序键：最近活跃（last_event_at 降序，PRD §3.1）。
pub fn session_sort_key(m: &SessionMeta) -> u64 {
    m.last_event_at
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SessionState;

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

    #[test]
    fn session_sort_key_uses_last_event() {
        let a = SessionMeta {
            id: "a".into(),
            harness: "codex".into(),
            cwd: "/".into(),
            model: None,
            state: SessionState::Idle,
            interrupted: false,
            closed: false,
            title: "a".into(),
            created_at: 1,
            last_event_at: 10,
        };
        let mut b = a.clone();
        b.id = "b".into();
        b.last_event_at = 20;
        b.title = "b".into();
        assert!(session_sort_key(&b) > session_sort_key(&a));
        // 排序：最近活跃在前
        let mut list = vec![a, b];
        list.sort_by_key(|s| std::cmp::Reverse(session_sort_key(s)));
        assert_eq!(list[0].id, "b");
    }
}
