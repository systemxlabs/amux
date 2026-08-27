//! Pure data transformations shared by the GUI and its tests.

use crate::config::RecentWorkspace;
use protocol::{Activity, ContentBlock, HistoryItem, SessionMeta};

/// 合并一条新近使用记录：(machine, workspace) 唯一、去重后移到最前、整体按最近使用降序、
/// 超出 `max` 时丢弃最旧。返回新的有序列表（最近使用在前）。
pub fn merge_recent_workspace(
    entries: &[RecentWorkspace],
    machine: &str,
    workspace: &str,
    now: u64,
    max: usize,
) -> Vec<RecentWorkspace> {
    let mut out: Vec<RecentWorkspace> = entries
        .iter()
        .filter(|e| !(e.machine == machine && e.workspace == workspace))
        .cloned()
        .collect();
    out.insert(
        0,
        RecentWorkspace {
            machine: machine.to_string(),
            workspace: workspace.to_string(),
            last_used: now,
        },
    );
    out.truncate(max);
    out
}

/// 某设备的常用工作目录路径（最近使用优先，按 `last_used` 降序稳定输出）。
pub fn recent_workspaces_for_machine(entries: &[RecentWorkspace], machine: &str) -> Vec<String> {
    let mut mine: Vec<&RecentWorkspace> = entries.iter().filter(|e| e.machine == machine).collect();
    mine.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
    mine.into_iter().map(|e| e.workspace.clone()).collect()
}

/// 把一窗会话并入已加载列表（首次取最近活跃一窗，滚动加载更早）。
/// - 首次加载：`existing` 为空 → 窗口即为当前列表
/// - 追加：更早一窗按序并入（保持最近活跃在前）
/// - 按 id 去重（并发刷新可能重复）
///
/// 返回（合并后的列表, 是否还有更早, 下次 before 游标）。
pub fn merge_session_window(
    existing: &[SessionMeta],
    window: Vec<SessionMeta>,
    has_more: bool,
    next_before: Option<String>,
) -> (Vec<SessionMeta>, bool, Option<String>) {
    let mut out = existing.to_vec();
    for m in window {
        if let Some(existing) = out.iter_mut().find(|s| s.id == m.id) {
            *existing = m;
        } else {
            out.push(m);
        }
    }
    (out, has_more, next_before)
}

/// 按最近活跃降序排序会话。
pub fn sort_sessions_recent(meta: &mut [SessionMeta]) {
    meta.sort_by_key(|entry| std::cmp::Reverse(entry.last_active_at));
}

/// 会话上下文占用展示文案（token）：`已用 / 窗口（百分比%）`。
/// 两者均为 0（尚未收到 `usage_update`）时返回 None（不展示）。
pub fn context_usage_text(used: u64, window: u64) -> Option<String> {
    if used == 0 && window == 0 {
        return None;
    }
    let used_s = format_thousands(used);
    if window == 0 {
        return Some(format!("{used_s} token"));
    }
    let window_s = format_thousands(window);
    let percent = (used as f64 / window as f64) * 100.0;
    Some(format!("{used_s} / {window_s} token（{percent:.1}%）"))
}

/// 会话上下文占用百分比（0.0–100.0）；窗口未知（0）时返回 None。
/// 供列表行迷你进度条使用。
pub fn context_percent(used: u64, window: u64) -> Option<f32> {
    if window == 0 {
        return None;
    }
    Some(((used as f64 / window as f64) * 100.0) as f32)
}

/// 千分位分组（仅用于展示，非契约）。
fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum DialogMsg {
    UserMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    AgentMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
}

/// 把 `session.history` 的对话历史条目变换为对话气泡列表（顺序保留：时间正序）。
pub fn history_to_dialog(items: &[HistoryItem]) -> Vec<DialogMsg> {
    items
        .iter()
        .map(|it| match it {
            HistoryItem::UserMessage { content, timestamp } => DialogMsg::UserMessage {
                content: content.clone(),
                timestamp: *timestamp,
            },
            HistoryItem::AgentMessage { content, timestamp } => DialogMsg::AgentMessage {
                content: content.clone(),
                timestamp: *timestamp,
            },
        })
        .collect()
}

/// 活动 → （种类标签, 详情文案）纯逻辑（渲染层做样式）。
pub fn activity_kind_detail(a: &Activity) -> (String, String) {
    match a {
        Activity::Thinking { content, .. } => ("思考".to_string(), content.clone()),
        Activity::ToolCall {
            name,
            title,
            content,
            ..
        } => {
            let title = title.clone().unwrap_or_default();
            let body = content.clone().unwrap_or_default();
            let combined = if title.trim().is_empty() {
                body
            } else if body.trim().is_empty() {
                title
            } else {
                format!("{title}\n{body}")
            };
            (format!("工具调用：{name}"), combined)
        }
        Activity::Compaction { detail, .. } => ("上下文压缩".to_string(), detail.clone()),
        Activity::Error { detail, .. } => ("错误".to_string(), detail.clone()),
    }
}

/// 输入附件：@ 引用文件/目录、拖拽文件/图片。
/// 图片以路径引用传递（不读二进制内容）——主流 agent CLI 自身具备按路径读取
/// 图片的能力，GUI 侧 read_to_string 二进制只会得到空串（曾为坏路径）。
#[derive(Debug, Clone, PartialEq)]
pub enum InputAttachment {
    Path {
        path: String,
        is_dir: bool,
    },
    /// 图片附件：拖拽/引用图片时读字节并 base64 编码，作为 ACP resource 传给 agent
    Image {
        name: String,
        mime_type: String,
        data_base64: String,
    },
}

/// 路径 → 路径附件，供拖拽文件/目录与 `@` 引用共用。
pub fn path_attachment(path: &str) -> InputAttachment {
    InputAttachment::Path {
        path: path.to_string(),
        is_dir: std::path::Path::new(path).is_dir(),
    }
}

/// 外部拖拽路径转换为附件；常见图片直接编码为 ACP resource，
/// 其他文件保持路径上下文附件。
pub fn external_path_attachment(path: &str) -> InputAttachment {
    let p = std::path::Path::new(path);
    let mime_type = match p
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        _ => None,
    };
    if let Some(mime_type) = mime_type {
        if let Ok(data) = std::fs::read(p) {
            use base64::Engine;
            return InputAttachment::Image {
                name: p
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path)
                    .to_string(),
                mime_type: mime_type.to_string(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            };
        }
    }
    path_attachment(path)
}

/// 解析输入文本中的 @ 引用，将文件或目录作为上下文。
/// 返回（清理后的文本，引用列表）。
pub fn parse_at_references(text: &str) -> (String, Vec<String>) {
    let mut refs = Vec::new();
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find('@') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '@')
            .unwrap_or(after.len());
        let path = after[..end].trim();
        if path.is_empty() {
            out.push('@');
            rest = after;
            continue;
        }
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

/// 常见图片扩展名（按路径引用传递、不读内容）。
fn is_image_path(path: &str) -> bool {
    matches!(
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg")
    )
}

/// 读取 @ 引用路径为上下文文本。图片文件只传路径引用（二进制读成文本无意义）。
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
    } else if is_image_path(path) {
        format!("[图片 {path}]（请用工具按此路径读取图片）")
    } else if p.is_file() {
        let content = std::fs::read_to_string(p).unwrap_or_default();
        let excerpt: String = content.chars().take(4000).collect();
        format!("[文件 {path}]\n{excerpt}")
    } else {
        format!("[引用不存在 {path}]")
    }
}

/// 附件 → prompt 内容块。
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

/// 工作流编排器当前使用文本上下文；将附件内容显式带入工作流 transcript，
/// 避免工作流输入丢失 `@` 引用和拖拽附件。
pub fn compose_workflow_text(text: &str, attachments: &[InputAttachment]) -> String {
    let mut result = text.to_string();
    for attachment in attachments {
        let detail = match attachment {
            InputAttachment::Path { path, .. } => read_path_context(path),
            InputAttachment::Image {
                name, mime_type, ..
            } => format!("[图片 {name}，类型 {mime_type}]"),
        };
        if !result.trim().is_empty() {
            result.push_str("\n\n");
        }
        result.push_str("[上下文附件]\n");
        result.push_str(&detail);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MAX_RECENT_WORKSPACES;
    use protocol::SessionState;

    fn rw(machine: &str, workspace: &str, last_used: u64) -> RecentWorkspace {
        RecentWorkspace {
            machine: machine.into(),
            workspace: workspace.into(),
            last_used,
        }
    }

    #[test]
    fn merge_recent_workspace_dedup_sort_cap() {
        let mut list = vec![
            rw("m1", "/a", 100),
            rw("m1", "/b", 200),
            rw("m1", "/c", 300),
        ];
        list = merge_recent_workspace(&list, "m1", "/a", 400, MAX_RECENT_WORKSPACES);
        let paths: Vec<&str> = list.iter().map(|e| e.workspace.as_str()).collect();
        assert_eq!(paths, ["/a", "/b", "/c"]);
        assert_eq!(list[0].last_used, 400);
        assert_eq!(list.len(), 3);
        let uniq: std::collections::HashSet<_> = list
            .iter()
            .map(|e| format!("{}:{}", e.machine, e.workspace))
            .collect();
        assert_eq!(uniq.len(), list.len());
        let mut capped = Vec::new();
        for t in 0..25 {
            capped = merge_recent_workspace(&capped, "m1", &format!("/d{t}"), t, 20);
        }
        assert_eq!(capped.len(), 20);
    }

    /// 按机器取最近工作目录，且不同机器互不混叠。
    #[test]
    fn recent_workspaces_for_machine_filters_and_sorts() {
        let list = vec![
            rw("m1", "/old", 100),
            rw("m1", "/new", 300),
            rw("m1", "/mid", 200),
            rw("m2", "/x", 400),
        ];
        assert_eq!(
            recent_workspaces_for_machine(&list, "m1"),
            vec!["/new", "/mid", "/old"]
        );
        assert_eq!(
            recent_workspaces_for_machine(&list, "nope"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn context_usage_text_formats_and_hides_unknown() {
        // 尚未收到 usage_update：两者为 0 → 不展示
        assert_eq!(context_usage_text(0, 0), None);
        // 仅窗口已知：只展示已用
        assert_eq!(
            context_usage_text(0, 200_000),
            Some("0 / 200,000 token（0.0%）".into())
        );
        // 典型占用：展示已用 / 窗口 与百分比
        assert_eq!(
            context_usage_text(53_000, 200_000),
            Some("53,000 / 200,000 token（26.5%）".into())
        );
        // 窗口未知：只展示已用量
        assert_eq!(context_usage_text(1_234, 0), Some("1,234 token".into()));
    }

    #[test]
    fn context_percent_ratio_or_none() {
        assert_eq!(context_percent(0, 0), None);
        assert_eq!(context_percent(1_234, 0), None);
        let p = context_percent(50_000, 200_000).unwrap();
        assert!((p - 25.0).abs() < 0.01, "50k/200k 应为 25%: {p}");
        let p = context_percent(200_000, 200_000).unwrap();
        assert!((p - 100.0).abs() < 0.01);
    }

    fn smeta(id: &str, last: u64) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            agent: "codex".into(),
            cwd: "/tmp".into(),
            state: SessionState::Idle,
            title: String::new(),
            created_at: 1,
            last_active_at: last,
            worktree_dir: String::new(),
            context_size: 0,
            context_window_size: 0,
            config_options: Vec::new(),
        }
    }

    #[test]
    fn merge_session_window_first_append_dedup() {
        let window1 = vec![smeta("s3", 300), smeta("s2", 200)];
        let (list, has_more, next) =
            merge_session_window(&[], window1, true, Some("200:s2".into()));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "s3");
        assert!(has_more);
        assert_eq!(next.as_deref(), Some("200:s2"));

        let window2 = vec![smeta("s1", 100)];
        let (list, has_more, next) = merge_session_window(&list, window2, false, None);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1"]
        );
        assert!(!has_more);
        assert_eq!(next, None);

        let window3 = vec![smeta("s2", 200), smeta("s0", 50)];
        let (list, _, _) = merge_session_window(&list, window3, false, None);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1", "s0"]
        );
    }

    #[test]
    fn sort_sessions_recent_desc() {
        let mut list = vec![smeta("a", 100), smeta("b", 300), smeta("c", 200)];
        sort_sessions_recent(&mut list);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
    }

    #[test]
    fn history_to_dialog_maps_all_kinds() {
        let items = vec![
            HistoryItem::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "你好".into(),
                }],
                timestamp: 1,
            },
            HistoryItem::AgentMessage {
                content: vec![ContentBlock::Text {
                    text: "回复".into(),
                }],
                timestamp: 2,
            },
        ];
        let dialog = history_to_dialog(&items);
        assert_eq!(dialog.len(), 2);
        assert!(matches!(&dialog[0], DialogMsg::UserMessage { .. }));
        assert!(matches!(&dialog[1], DialogMsg::AgentMessage { .. }));
    }

    #[test]
    fn activity_kind_detail_maps_variants() {
        let thinking = Activity::Thinking {
            timestamp: 1,
            content: "思考中".into(),
        };
        assert_eq!(
            activity_kind_detail(&thinking),
            ("思考".into(), "思考中".into())
        );
        let tool = Activity::ToolCall {
            timestamp: 2,
            name: "execute".into(),
            title: Some("运行".into()),
            content: Some("cargo test".into()),
        };
        assert_eq!(
            activity_kind_detail(&tool),
            ("工具调用：execute".into(), "运行\ncargo test".into())
        );
        let err = Activity::Error {
            timestamp: 3,
            detail: "失败".into(),
        };
        assert_eq!(activity_kind_detail(&err), ("错误".into(), "失败".into()));
    }

    #[test]
    fn parse_at_references_extracts_paths_and_cleans_text() {
        let (text, refs) = parse_at_references("用 @src/main.rs 的代码实现功能");
        assert_eq!(text, "用  的代码实现功能");
        assert_eq!(refs, vec!["src/main.rs"]);
        let (text, refs) = parse_at_references("联系 a@b.com 或 @ ");
        assert_eq!(refs.len(), 0);
        assert_eq!(text, "联系 a@b.com 或 @ ");
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
        let blocks = compose_prompt(
            "  ",
            &[InputAttachment::Path {
                path: "x".into(),
                is_dir: false,
            }],
        );
        assert_eq!(blocks.len(), 1);
    }
}
