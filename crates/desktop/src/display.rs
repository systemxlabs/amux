//! 共享展示助手（不依赖 `AmuxApp` 状态；仅依赖 GPUI/协议类型）。

use gpui::*;
use gpui_component::{label::Label, *};

use protocol::Activity;

use crate::logic::activity_kind_detail;

/// 工作目录短名：取路径最后一段。
pub fn short_cwd(cwd: &str) -> String {
    cwd.rsplit('/').next().unwrap_or(cwd).to_string()
}

/// 活动 → （标签，详情）文案（样式逻辑委托给纯函数）。
pub fn activity_display(a: &Activity) -> (String, String) {
    activity_kind_detail(a)
}

/// 机器状态徽章。
pub fn machine_status_badge(status: &str) -> impl IntoElement {
    Label::new(status.to_string())
        .text_xs()
        .text_color(color_for_status(status))
}

fn color_for_status(status: &str) -> Hsla {
    if status.starts_with("已连接") || status.starts_with("认证成功") {
        rgb(0x16a34a).into()
    } else if status.starts_with("连接失败") || status.starts_with("认证失败") {
        rgb(0xb91c1c).into()
    } else {
        rgb(0xd97706).into()
    }
}

/// 信息行：灰标签 + 值。
pub fn info_row(label: &str, value: &str) -> impl IntoElement {
    h_flex()
        .gap_2()
        .child(
            Label::new(format!("{}：", label))
                .text_sm()
                .text_color(rgb(0x6b7280)),
        )
        .child(
            Label::new(value.to_string())
                .text_sm()
                .text_color(rgb(0x111827)),
        )
}
