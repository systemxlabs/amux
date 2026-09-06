//! 共享展示助手（不依赖 `AmuxApp` 状态；仅依赖 GPUI/协议类型）。

use gpui::*;
use gpui_component::{label::Label, tag::Tag, *};

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

/// 机器状态徽章：连接状态 + 可选的操作级提示（notice 存在时覆盖显示、警示色）。
pub fn machine_status_badge<'a>(
    state: (&'a crate::machine::MachineStatus, Option<&'a str>),
    success: Hsla,
    danger: Hsla,
    warning: Hsla,
) -> impl IntoElement {
    let (status, notice) = state;
    let (text, color) = match notice {
        Some(n) => (n.to_string(), warning),
        None => (
            status.label(),
            color_for_status(status, success, danger, warning),
        ),
    };
    // 药丸徽章：底色为状态色低透明度、前景/描边用状态色本身（浅底上可读）
    Tag::custom(color.opacity(0.14), color, color.opacity(0.35))
        .small()
        .rounded_full()
        .max_w(px(180.)) // 徽章文本截断上限（小标签固定宽度）
        .child(Label::new(text).truncate())
}

fn color_for_status(
    status: &crate::machine::MachineStatus,
    success: Hsla,
    danger: Hsla,
    warning: Hsla,
) -> Hsla {
    match status {
        crate::machine::MachineStatus::Online => success,
        crate::machine::MachineStatus::AuthFailed(_) => danger,
        crate::machine::MachineStatus::Connecting | crate::machine::MachineStatus::Offline => {
            warning
        }
    }
}

/// 信息行：灰标签 + 值。
pub fn info_row(
    label: &str,
    value: &str,
    muted_foreground: Hsla,
    foreground: Hsla,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_start()
        .child(
            Label::new(format!("{}：", label))
                .text_sm()
                .flex_none()
                .text_color(muted_foreground),
        )
        .child(
            Label::new(value.to_string())
                .text_sm()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(foreground),
        )
}

#[cfg(test)]
mod tests {
    use super::short_cwd;

    #[test]
    fn short_cwd_handles_platform_separators_and_root() {
        assert_eq!(short_cwd("/home/user/project"), "project");
        assert_eq!(short_cwd(r"C:\Users\me\project"), "project");
        assert_eq!(short_cwd("/"), "/");
        assert_eq!(short_cwd(""), "");
    }
}
