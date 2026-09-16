//! 共享渲染助手：时间格式化、气泡宽度估算、活动文案、状态标签与空态。

use amux_common::domain::{Activity, ContentBlock, SessionConfigKind, SessionConfigOption};
use gpui::*;
use gpui_component::label::Label;
use gpui_component::tag::Tag;
use gpui_component::{h_flex, v_flex, Icon, IconName, Sizable};


/// 主题取色的值拷贝。
///
/// 视图函数在同一个函数体内既要读主题、又要可变借用 `Context`（事件回调），
/// 直接持有 `&Theme` 会被借用检查拒绝，故统一先拷贝成值再用。
#[derive(Clone)]
pub struct Colors {
    pub background: Hsla,
    pub foreground: Hsla,
    pub border: Hsla,
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub popover: Hsla,
    pub muted: Hsla,
    pub muted_foreground: Hsla,
    pub accent: Hsla,
    pub secondary_hover: Hsla,
    pub list_active: Hsla,
    pub list_active_border: Hsla,
    pub list_hover: Hsla,
    pub sidebar: Hsla,
    pub sidebar_border: Hsla,
    pub danger: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub warning_foreground: Hsla,
    pub overlay: Hsla,
    pub mono_font_family: SharedString,
}

impl Colors {
    pub fn of(theme: &gpui_component::Theme) -> Self {
        Self {
            background: theme.background,
            foreground: theme.foreground,
            border: theme.border,
            primary: theme.primary,
            primary_foreground: theme.primary_foreground,
            popover: theme.popover,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            accent: theme.accent,
            secondary_hover: theme.secondary_hover,
            list_active: theme.list_active,
            list_active_border: theme.list_active_border,
            list_hover: theme.list_hover,
            sidebar: theme.sidebar,
            sidebar_border: theme.sidebar_border,
            danger: theme.danger,
            success: theme.success,
            warning: theme.warning,
            warning_foreground: theme.warning_foreground,
            overlay: theme.overlay,
            mono_font_family: theme.mono_font_family.clone(),
        }
    }
}

/// 时间精度档：秒级（对话/活动）或分级紧凑（会话列表行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimePrecision {
    /// `HH:MM:SS`；跨天附 `MM-DD`。
    Seconds,
    /// `HH:MM`；跨天只显示日期。
    Compact,
}

/// 本地时区时间戳格式化；时间戳非法时返回空串。
pub fn format_local_time(timestamp_ms: u64, precision: TimePrecision) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(timestamp_ms as i64) else {
        return String::new();
    };
    let ts = ts.to_zoned(jiff::tz::TimeZone::system());
    let same_day = ts.date() == jiff::Zoned::now().date();
    match precision {
        TimePrecision::Seconds => {
            let time = ts.strftime("%H:%M:%S").to_string();
            if same_day {
                time
            } else {
                format!("{} {}", ts.strftime("%m-%d"), time)
            }
        }
        TimePrecision::Compact => {
            if same_day {
                ts.strftime("%H:%M").to_string()
            } else {
                ts.strftime("%m-%d").to_string()
            }
        }
    }
}

/// 折叠所有空白为单个空格（单行展示用）；不设字符上限，超长由布局截断。
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按字符截断，超出部分以省略号结尾（列表行等固定宽度场景）。
pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// 内容块列表 → 文本；非文本块忽略，多个文本块以换行分隔。
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 对话气泡中的图片附件：解码 `Resource` 块的 base64 blob。
pub fn message_images(blocks: &[ContentBlock]) -> Vec<Image> {
    use base64::Engine as _;
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Resource {
                mime_type,
                blob: Some(blob),
                ..
            } => {
                let format = match mime_type.as_str() {
                    "image/png" => ImageFormat::Png,
                    "image/jpeg" => ImageFormat::Jpeg,
                    "image/gif" => ImageFormat::Gif,
                    "image/webp" => ImageFormat::Webp,
                    "image/bmp" => ImageFormat::Bmp,
                    _ => return None,
                };
                let bytes = base64::engine::general_purpose::STANDARD.decode(blob).ok()?;
                Some(Image::from_bytes(format, bytes))
            }
            _ => None,
        })
        .collect()
}

/// CJK 等全角字符判定（近似）。
fn is_cjk_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
}

/// 气泡宽度：按最长行估算（全角计 1em、其余计 0.6em），夹在 `[min, max]`。
///
/// 逐帧做真实文本整形代价高，近似即可让短消息贴内容收拢、长消息触顶换行；
/// 不估算则气泡恒为上限宽。
pub fn estimate_bubble_width(text: &str, font_px: f32, min: f32, max: f32) -> Pixels {
    // 气泡左右内边距 + 圆角内呼吸空间
    const PAD: f32 = 28.0;
    let widest_em = text
        .lines()
        .map(|line| {
            line.chars()
                .map(|c| if is_cjk_wide(c) { 1.0 } else { 0.6 })
                .sum::<f32>()
        })
        .fold(0.0_f32, f32::max);
    px((widest_em * font_px + PAD).clamp(min, max))
}

/// 工作目录短名：取路径最后一段。
pub fn short_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return cwd.to_string();
    }
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// agent 可用状态药丸标签。
pub fn availability_tag(available: bool) -> impl IntoElement {
    let tag = if available {
        Tag::success()
    } else {
        Tag::danger()
    };
    tag.small()
        .rounded_full()
        .child(Label::new(if available { "可用" } else { "不可用" }).text_xs())
}

/// 详情面板信息行：灰标签 + 值（值单行截断）。
pub fn info_row(label: &str, value: &str, theme: &Colors) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_start()
        .child(
            Label::new(format!("{label}："))
                .text_sm()
                .flex_none()
                .text_color(theme.muted_foreground),
        )
        .child(
            Label::new(value.to_string())
                .text_sm()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.foreground),
        )
}

/// 活动 → （种类标签, 详情文案）。
pub fn activity_kind_detail(activity: &Activity) -> (String, String) {
    match activity {
        Activity::Thinking { thinking, .. } => ("思考".to_string(), thinking.clone()),
        Activity::ToolCall {
            tool_name,
            title,
            parameters,
            ..
        } => {
            let title = title.clone().unwrap_or_default();
            let body = parameters.clone().unwrap_or_default();
            let detail = if title.trim().is_empty() {
                body
            } else if body.trim().is_empty() {
                title
            } else {
                format!("{title}\n{body}")
            };
            (format!("工具调用：{tool_name}"), detail)
        }
        Activity::Error { error, .. } => ("错误".to_string(), error.clone()),
    }
}

/// 实时活动条文案（无进行中活动为 `None`）；内容只折叠空白，由布局截断。
pub fn activity_bar_text(current: Option<&Activity>) -> Option<String> {
    match current? {
        Activity::Thinking { thinking, .. } => Some(format!("思考中：{}", one_line(thinking))),
        Activity::ToolCall {
            tool_name, title, ..
        } => Some(format!(
            "工具调用：{} {}",
            tool_name,
            one_line(title.as_deref().unwrap_or(""))
        )),
        Activity::Error { error, .. } => Some(format!("错误：{}", one_line(error))),
    }
}

/// 活动时间戳。
pub fn activity_timestamp(activity: &Activity) -> u64 {
    match activity {
        Activity::Thinking { timestamp, .. } => *timestamp,
        Activity::ToolCall { timestamp, .. } => *timestamp,
        Activity::Error { timestamp, .. } => *timestamp,
    }
}

/// 会话选项 select 的当前值展示名（找不到时回退到原始值）。
pub fn config_value_label(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select {
            current_value,
            options,
        } => options
            .iter()
            .find(|entry| &entry.value == current_value)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| current_value.clone()),
        SessionConfigKind::Boolean { current_value } => current_value.to_string(),
    }
}

/// 会话上下文占用文案（token）；两者均为 0（尚未收到 usage 通知）时为 `None`。
pub fn context_usage_text(used: u64, window: u64) -> Option<String> {
    if used == 0 && window == 0 {
        return None;
    }
    if window == 0 {
        return Some(format!("{} token", thousands(used)));
    }
    let percent = (used as f64 / window as f64) * 100.0;
    Some(format!(
        "{} / {} token（{percent:.1}%）",
        thousands(used),
        thousands(window)
    ))
}

fn thousands(value: u64) -> String {
    let text = value.to_string();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }
    out
}

/// 居中空态：图标 + 文案。
pub fn empty_state(hint: &str, icon: IconName, theme: &Colors) -> impl IntoElement {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_2()
        .child(
            Icon::new(icon)
                .large()
                .text_color(theme.muted_foreground.opacity(0.55)),
        )
        .child(
            Label::new(hint.to_string())
                .text_sm()
                .text_color(theme.muted_foreground),
        )
}

/// 面板内联的空态提示文案。
pub fn empty_hint(text: &str, theme: &Colors) -> impl IntoElement {
    div()
        .p_2()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(text.to_string())
}
