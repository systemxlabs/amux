//! 共享展示助手（不依赖 `AmuxApp` 状态；仅依赖 GPUI/协议类型）。

use gpui::*;
use gpui_component::{label::Label, *};

use protocol::Activity;

/// 工作目录短名：取路径最后一段。
pub fn short_cwd(cwd: &str) -> String {
    cwd.rsplit('/').next().unwrap_or(cwd).to_string()
}

/// 活动种类 → （标签，详情）文案。
pub fn activity_display(a: &Activity) -> (String, String) {
    match a {
        Activity::Thinking { content, .. } => ("思考".into(), content.clone()),
        Activity::ToolCall {
            name,
            title,
            content,
            ..
        } => (
            "工具调用".into(),
            format!(
                "{} {} {}",
                name,
                title.clone().unwrap_or_default(),
                content.clone().unwrap_or_default()
            ),
        ),
        Activity::Compaction { detail, .. } => ("压缩".into(), detail.clone()),
    }
}

/// 机器在线状态徽章（PRD §3.3 在线状态）。
pub fn machine_status_badge(status: &str) -> impl IntoElement {
    let color = if status.starts_with("已连接") {
        rgb(0x16a34a)
    } else if status.starts_with("连接失败") || status.starts_with("离线") {
        rgb(0xdc2626)
    } else {
        rgb(0x9ca3af)
    };
    Label::new(status).text_xs().text_color(color)
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
