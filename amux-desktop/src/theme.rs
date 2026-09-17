//! 桌面端主题：颜色/圆角/字号取自项目根 theme.json（docs/DESIGN.md「共享主题」，
//! 编译期内嵌，与 Web 端同一来源），映射到 gpui-component 的语义主题；
//! 三面板布局与各控件一律引用这些令牌，不写死颜色。

use gpui::{px, App, Hsla, Rgba, Window};
use gpui_component::{Theme, ThemeTokens};
use serde::Deserialize;

/// 左侧面板默认宽度（可拖拽调整）。
pub const SIDEBAR_WIDTH: f32 = 240.0;

/// 编译期内嵌的共享主题（docs/DESIGN.md「共享主题」），与 Web 端同一文件。
const SHARED_JSON: &str = include_str!("../../theme.json");

/// 解析后的共享主题；主题坏了应用无法按预期呈现，直接 panic。
fn shared() -> Shared {
    serde_json::from_str(SHARED_JSON).expect("theme.json 解析失败")
}

#[derive(Deserialize)]
struct Shared {
    color: Colors,
    radius: Radius,
    font: Font,
}

/// theme.json 的 camelCase 颜色键，与 Web 端 CSS 变量一一对应。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Colors {
    background: String,
    foreground: String,
    popover: String,
    popover_foreground: String,
    primary: String,
    primary_foreground: String,
    secondary: String,
    secondary_foreground: String,
    muted: String,
    muted_foreground: String,
    accent: String,
    accent_foreground: String,
    destructive: String,
    destructive_foreground: String,
    border: String,
    input: String,
    ring: String,
    scrollbar_thumb: String,
    scrollbar_thumb_hover: String,
    link: String,
    success: String,
    success_foreground: String,
    warning: String,
    warning_foreground: String,
    diff_add: String,
    diff_remove: String,
}

#[derive(Deserialize)]
struct Radius {
    #[allow(dead_code)] // radius-sm 由 Web 端细粒度组件使用，桌面端只消费 md/lg
    sm: u32,
    md: u32,
    lg: u32,
}

#[derive(Deserialize)]
struct Font {
    body: u32,
}

/// 正文字号（px）；同时决定 `rem` 基准（Root 每帧把 rem 设为该值）。
pub fn font_body() -> gpui::Pixels {
    px(shared().font.body as f32)
}

/// #rrggbb → Hsla；非法色值在启动时 panic（主题坏了应用必然异常）。
fn color(hex: &str) -> Hsla {
    let value = hex.strip_prefix('#').expect("theme.json 颜色需为 #rrggbb");
    let int = u32::from_str_radix(value, 16).expect("theme.json 颜色需为 #rrggbb");
    Rgba {
        r: ((int >> 16) & 0xff) as f32 / 255.0,
        g: ((int >> 8) & 0xff) as f32 / 255.0,
        b: (int & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

fn translucent(hex: &str, alpha: f32) -> Hsla {
    let mut value = color(hex);
    value.a = alpha;
    value
}

/// 应用共享主题到 gpui-component：浅色一套，按 hover/active 派生交互色。
pub fn apply(cx: &mut App) {
    let shared = shared();
    let c = &shared.color;
    let theme = Theme::global_mut(cx);
    theme.background = color(&c.background);
    theme.foreground = color(&c.foreground);
    theme.border = color(&c.border);
    theme.ring = color(&c.ring);
    theme.sidebar = color(&c.muted);
    theme.sidebar_foreground = color(&c.muted_foreground);
    theme.sidebar_border = color(&c.border);
    theme.sidebar_accent = color(&c.accent);
    theme.sidebar_accent_foreground = color(&c.accent_foreground);
    theme.sidebar_primary = color(&c.primary);
    theme.sidebar_primary_foreground = color(&c.primary_foreground);
    theme.list.active_highlight = true;
    theme.list_hover = color(&c.accent);
    theme.list_active = color(&c.secondary);
    theme.list_active_border = color(&c.border);
    theme.list_even = color(&c.background);
    theme.list_head = color(&c.background);
    theme.muted = color(&c.muted);
    theme.muted_foreground = color(&c.muted_foreground);
    theme.accent = color(&c.accent);
    theme.accent_foreground = color(&c.accent_foreground);
    theme.primary = color(&c.primary);
    theme.primary_hover = color(&c.primary).opacity(0.9);
    theme.primary_active = color(&c.primary).opacity(0.8);
    theme.primary_foreground = color(&c.primary_foreground);
    theme.secondary = color(&c.secondary);
    theme.secondary_hover = color(&c.secondary).opacity(0.9);
    theme.secondary_active = color(&c.secondary).opacity(0.8);
    theme.secondary_foreground = color(&c.secondary_foreground);
    theme.popover = color(&c.popover);
    theme.popover_foreground = color(&c.popover_foreground);
    theme.group_box = color(&c.popover);
    theme.group_box_foreground = color(&c.popover_foreground);
    theme.input = color(&c.input);
    theme.selection = translucent(&c.primary, 0.16);
    theme.caret = color(&c.foreground);
    theme.title_bar = color(&c.secondary);
    theme.title_bar_border = color(&c.secondary);
    theme.scrollbar_thumb = color(&c.scrollbar_thumb);
    theme.scrollbar_thumb_hover = color(&c.scrollbar_thumb_hover);
    theme.link = color(&c.link);
    // 语义文字色需在浅色底上保持可读：这里是状态标签与行内校验文案的前景色
    theme.danger = color(&c.destructive);
    theme.danger_hover = color(&c.destructive).opacity(0.9);
    theme.danger_active = color(&c.destructive).opacity(0.8);
    theme.danger_foreground = color(&c.destructive_foreground);
    theme.success = color(&c.success);
    theme.success_foreground = color(&c.success_foreground);
    theme.warning = color(&c.warning);
    theme.warning_foreground = color(&c.warning_foreground);
    theme.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.25);
    theme.font_family = ".AppleSystemUIFont".into();
    theme.font_size = font_body();
    // radius-md 作为组件默认圆角、radius-lg 作对话框等大元素圆角、radius-sm 作小控件圆角
    theme.radius = px(shared.radius.md as f32);
    theme.radius_lg = px(shared.radius.lg as f32);
    theme.shadow = true;
    // 组件读取的是由颜色派生的 tokens：改完颜色必须重建，否则组件仍用默认配色
    theme.tokens = ThemeTokens::from(&theme.colors);
}

/// 打开应用时套用共享主题；浅色为唯一模式（docs/DESIGN.md「共享主题」只有一套主题）。
pub fn sync_appearance(_window: Option<&mut Window>, cx: &mut App) {
    apply(cx);
}

/// 改动审查行的 diff 底色：按行类别取自共享主题（与 Web 端同一取值）。
pub fn diff_line_background(kind: amux_common::domain::GitDiffLineKind) -> Hsla {
    let shared = shared();
    match kind {
        amux_common::domain::GitDiffLineKind::Add => color(&shared.color.diff_add),
        amux_common::domain::GitDiffLineKind::Remove => color(&shared.color.diff_remove),
        amux_common::domain::GitDiffLineKind::Context => gpui::transparent_black(),
    }
}
