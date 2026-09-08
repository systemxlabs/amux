//! Pure data transformations shared by the GUI and its tests.

use std::collections::BTreeMap;

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
pub fn merge_session_window(
    existing: &[SessionMeta],
    window: Vec<SessionMeta>,
) -> Vec<SessionMeta> {
    let mut out = existing.to_vec();
    for m in window {
        if let Some(existing) = out.iter_mut().find(|s| s.id == m.id) {
            *existing = m;
        } else {
            out.push(m);
        }
    }
    out
}

/// 从指定机器的普通会话查询结果中移除所有已知工作流关联会话。
/// 关联会话只允许挂在所属工作流下展示；机器名是会话身份的一部分，
/// 因此不能只按 session ID 过滤。
pub fn filter_workflow_sessions(
    sessions: Vec<SessionMeta>,
    machine_name: &str,
    workflow_linked_sessions: &std::collections::HashSet<(String, String)>,
) -> Vec<SessionMeta> {
    sessions
        .into_iter()
        .filter(|session| {
            !workflow_linked_sessions.contains(&(machine_name.to_string(), session.id.clone()))
        })
        .collect()
}

/// 关联会话无法从所属机器获取时的固定展示标题。
pub fn unavailable_workflow_session_title(session_id: &str, machine_name: &str) -> String {
    format!("异常会话 {}@{}", session_id, machine_name)
}

/// 按最近活跃降序排序会话。
pub fn sort_sessions_recent(meta: &mut [SessionMeta]) {
    meta.sort_by_key(|entry| std::cmp::Reverse(entry.last_active_at));
}

/// 仅当输入以 `/` 开头且命令名 token 尚未输入完（`/` 后无空白）时返回
/// `/` 之后的已输入前缀；其余（非 `/` 开头、含空白、正文提及 `/`）返回 None。
pub fn slash_command_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// 前缀匹配斜杠命令（大小写不敏感，保持原顺序）。
pub fn filter_slash_commands<'a>(
    commands: &'a [protocol::SlashCommand],
    prefix: &str,
) -> Vec<&'a protocol::SlashCommand> {
    let p = prefix.to_lowercase();
    commands
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&p))
        .collect()
}

/// 把输入值按最后一个路径分隔符拆成「父目录 + 前缀」，供工作目录输入联想：
/// 仅父目录是绝对路径（以 `/` 开头）时返回——相对路径/`~` 无法直接发 `fs.list`；
/// 前缀为空（输入以 `/` 结尾）表示联想父目录的全部下一级目录项。
pub fn cwd_completion_target(input: &str) -> Option<(String, String)> {
    let (parent, prefix) = input.trim().rsplit_once('/')?;
    let parent = if parent.is_empty() { "/" } else { parent };
    if !parent.starts_with('/') {
        return None;
    }
    Some((parent.to_string(), prefix.to_string()))
}

/// 工作目录联想过滤（应用侧，server 的 `fs.list` 只负责分页列目录）：
/// 文件与目录都保留，按名称前缀匹配（大小写不敏感，保持 server 返回顺序）。
pub fn filter_cwd_suggestions(
    entries: &[protocol::FsEntry],
    prefix: &str,
) -> Vec<protocol::FsEntry> {
    let p = prefix.to_lowercase();
    entries
        .iter()
        .filter(|e| e.name.to_lowercase().starts_with(p.as_str()))
        .cloned()
        .collect()
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

/// 输入附件：拖拽文件/图片。
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

/// 路径 → 路径附件，供拖拽文件/目录使用。
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

/// 将原始图片字节编码为 ACP resource 附件。
pub fn image_attachment(name: &str, mime_type: &str, bytes: &[u8]) -> InputAttachment {
    use base64::Engine;
    InputAttachment::Image {
        name: name.to_string(),
        mime_type: mime_type.to_string(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
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

/// 读取路径为上下文文本。图片文件只传路径引用（二进制读成文本无意义）。
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
        // 限量读：只取开头若干字节（4000 字符的 UTF-8 上界），大文件不整读进内存
        use std::io::Read;
        let mut bytes = Vec::new();
        if let Ok(f) = std::fs::File::open(p) {
            let _ = f.take(16_384).read_to_end(&mut bytes);
        }
        let content = String::from_utf8_lossy(&bytes);
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
/// 避免工作流输入丢失拖拽附件。
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

/// 改动审查视图左侧文件树的目录节点：按目录层级组织成树，
/// 树中只出现包含改动文件的目录。
#[derive(Debug, PartialEq, Eq)]
pub struct ChangedDirNode {
    /// 展示标签：单链合并节点为多级片段（如 "storage/s3"），普通节点为目录名
    pub label: String,
    /// 完整目录路径（相对仓库根），作为折叠状态键
    pub path: String,
    /// 直接位于本目录下的改动文件（diff 列表下标）
    pub files: Vec<usize>,
    /// 子目录，按名称排序
    pub children: Vec<ChangedDirNode>,
}

/// 把改动文件路径按目录层级构建为树；不含改动文件且只有一个子目录的
/// 中间目录与该子目录合并为单链节点（文档示例中的 `storage/s3`）。
pub fn build_changed_file_tree(paths: &[impl AsRef<str>]) -> Vec<ChangedDirNode> {
    struct Trie {
        files: Vec<usize>,
        dirs: BTreeMap<String, Trie>,
    }
    impl Default for Trie {
        fn default() -> Self {
            Trie {
                files: Vec::new(),
                dirs: BTreeMap::new(),
            }
        }
    }

    let mut root = Trie::default();
    for (path_index, path) in paths.iter().enumerate() {
        let segments: Vec<&str> = path.as_ref().split('/').collect();
        let mut cur = &mut root;
        for dir in &segments[..segments.len() - 1] {
            cur = cur.dirs.entry((*dir).to_string()).or_default();
        }
        cur.files.push(path_index);
    }

    fn convert(dirs: BTreeMap<String, Trie>, prefix: &str) -> Vec<ChangedDirNode> {
        dirs.into_iter()
            .map(|(name, child)| {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let Trie { files, dirs } = child;
                let children = convert(dirs, &path);
                let mut node = ChangedDirNode {
                    label: name,
                    path: path.clone(),
                    files,
                    children,
                };
                // 单链合并：本目录不含改动文件且只有一个子目录时整体并入该子目录
                while node.files.is_empty() && node.children.len() == 1 {
                    let child = node.children.pop().expect("children.len() == 1");
                    node.label = format!("{}/{}", node.label, child.label);
                    node.path = child.path;
                    node.files = child.files;
                    node.children = child.children;
                }
                node
            })
            .collect()
    }
    convert(root.dirs, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MAX_RECENT_WORKSPACES;
    use protocol::SessionState;

    #[test]
    fn changed_file_tree_builds_hierarchy_and_merges_chains() {
        // 对应 PRD 示例：src 下 catalog/storage 两级，storage/s3 为单链合并节点
        let paths = [
            "src/catalog/helper/query.rs",
            "src/catalog/schema.rs",
            "src/storage/s3/parquet.rs",
            "README.md",
        ];
        let tree = build_changed_file_tree(&paths);
        assert_eq!(tree.len(), 1, "根级只有 src 一个目录，README.md 是根级文件");
        let src = &tree[0];
        assert_eq!(src.label, "src");
        assert!(src.files.is_empty(), "README.md 是根级文件，不属于任何目录节点");
        assert_eq!(
            src.children.iter().map(|n| n.label.as_str()).collect::<Vec<_>>(),
            ["catalog", "storage/s3"]
        );
        let catalog = &src.children[0];
        assert_eq!(catalog.path, "src/catalog");
        assert_eq!(catalog.files, &[1]);
        let helper = &catalog.children[0];
        assert_eq!(helper.path, "src/catalog/helper");
        assert_eq!(helper.files, &[0]);
        let s3 = &src.children[1];
        assert_eq!(s3.label, "storage/s3");
        assert_eq!(s3.path, "src/storage/s3");
        assert_eq!(s3.files, &[2]);
        assert!(s3.children.is_empty());
    }

    #[test]
    fn changed_file_tree_merges_multi_segment_chains() {
        let tree = build_changed_file_tree(&["a/b/c/d.rs"]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].label, "a/b/c");
        assert_eq!(tree[0].path, "a/b/c");
        assert_eq!(tree[0].files, &[0]);
    }

    #[test]
    fn changed_file_tree_keeps_sibling_dirs_split() {
        // 单链合并只发生在"无文件且唯一子目录"的情形，多子目录不合并
        let tree = build_changed_file_tree(&["a/x/1.rs", "a/y/2.rs"]);
        let a = &tree[0];
        assert_eq!(a.label, "a");
        assert!(a.files.is_empty());
        assert_eq!(a.children.len(), 2);
        assert_eq!(a.children[0].label, "x");
        assert_eq!(a.children[1].label, "y");
    }

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
        }
    }

    #[test]
    fn merge_session_window_first_append_dedup() {
        let window1 = vec![smeta("s3", 300), smeta("s2", 200)];
        let list = merge_session_window(&[], window1);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "s3");

        let window2 = vec![smeta("s1", 100)];
        let list = merge_session_window(&list, window2);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1"]
        );

        let window3 = vec![smeta("s2", 200), smeta("s0", 50)];
        let list = merge_session_window(&list, window3);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1", "s0"]
        );
    }

    #[test]
    fn filter_workflow_sessions_removes_linked_sessions_from_top_level_results() {
        let sessions = vec![smeta("ordinary", 300), smeta("linked", 200)];
        let linked_sessions =
            std::collections::HashSet::from([("machine-a".to_string(), "linked".to_string())]);
        let filtered = filter_workflow_sessions(sessions, "machine-a", &linked_sessions);
        assert_eq!(
            filtered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["ordinary"]
        );
    }

    #[test]
    fn filter_workflow_sessions_keeps_same_id_on_another_machine() {
        let sessions = vec![smeta("same-id", 200)];
        let linked_sessions =
            std::collections::HashSet::from([("machine-a".to_string(), "same-id".to_string())]);
        let filtered = filter_workflow_sessions(sessions, "machine-b", &linked_sessions);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn unavailable_workflow_session_title_includes_id_and_machine() {
        assert_eq!(
            unavailable_workflow_session_title("s-1", "dev-box"),
            "异常会话 s-1@dev-box"
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

    #[test]
    fn slash_prefix_only_while_typing_command_token() {
        assert_eq!(slash_command_prefix("/"), Some(""));
        assert_eq!(slash_command_prefix("/go"), Some("go"));
        assert_eq!(slash_command_prefix("/GOAL"), Some("GOAL"));
        // 命令名输入完成（空白开启参数）后不再弹出上拉框
        assert_eq!(slash_command_prefix("/goal "), None);
        assert_eq!(slash_command_prefix("/goal 做点事"), None);
        assert_eq!(slash_command_prefix("/goal\n"), None);
        // 正文提及 / 或空输入不触发
        assert_eq!(slash_command_prefix("帮我 /goal"), None);
        assert_eq!(slash_command_prefix(""), None);
        assert_eq!(slash_command_prefix("path/is/here"), None);
    }

    #[test]
    fn filter_slash_commands_matches_prefix_case_insensitively() {
        let commands = vec![
            protocol::SlashCommand {
                name: "goal".into(),
                description: "目标".into(),
                hint: None,
            },
            protocol::SlashCommand {
                name: "Review".into(),
                description: "审查".into(),
                hint: None,
            },
        ];
        let names = |prefix: &str| {
            filter_slash_commands(&commands, prefix)
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(""), vec!["goal", "Review"]);
        assert_eq!(names("g"), vec!["goal"]);
        assert_eq!(names("RE"), vec!["Review"]);
        assert!(names("x").is_empty());
    }

    fn fs_entry(name: &str, is_dir: bool) -> protocol::FsEntry {
        protocol::FsEntry {
            name: name.into(),
            path: format!("/home/{name}"),
            is_dir,
            size: 0,
        }
    }

    #[test]
    fn cwd_completion_target_splits_parent_and_prefix() {
        // 根目录下的前缀
        assert_eq!(
            cwd_completion_target("/ho"),
            Some(("/".into(), "ho".into()))
        );
        // 多级父目录
        assert_eq!(
            cwd_completion_target("/home/li"),
            Some(("/home".into(), "li".into()))
        );
        // 以分隔符结尾：联想父目录的全部下一级目录
        assert_eq!(
            cwd_completion_target("/home/"),
            Some(("/home".into(), "".into()))
        );
        // 两端空白不影响解析
        assert_eq!(
            cwd_completion_target("  /home/li  "),
            Some(("/home".into(), "li".into()))
        );
        // 相对路径 / ~ / 空输入：不联想（离线手动输入或最近目录场景）
        assert_eq!(cwd_completion_target("home/li"), None);
        assert_eq!(cwd_completion_target("~/pro"), None);
        assert_eq!(cwd_completion_target(""), None);
        assert_eq!(
            cwd_completion_target("/home/li/pro"),
            Some(("/home/li".into(), "pro".into()))
        );
    }

    #[test]
    fn filter_cwd_suggestions_matches_prefix_case_insensitively() {
        let entries = vec![
            fs_entry("projects", true),
            fs_entry("Pictures", true),
            fs_entry("notes.txt", false),
        ];
        let names = |prefix: &str| {
            filter_cwd_suggestions(&entries, prefix)
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>()
        };
        // 空前缀：返回全部目录项（文件与目录）
        assert_eq!(names(""), vec!["projects", "Pictures", "notes.txt"]);
        // 前缀匹配（大小写不敏感）
        assert_eq!(names("pro"), vec!["projects"]);
        assert_eq!(names("pic"), vec!["Pictures"]);
        // 文件也参与匹配
        assert_eq!(names("not"), vec!["notes.txt"]);
        // 无匹配
        assert!(names("zzz").is_empty());
    }
}
