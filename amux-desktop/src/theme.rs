//! shadcn/ui Zinc 视觉令牌（浅色/深色两套）：覆盖 gpui-component 的语义主题，
//! 全局字号与圆角也在此统一。三面板布局与各控件一律引用这些令牌，不写死颜色。

use gpui::{px, App, Hsla, Rgba, Window};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode, ThemeTokens};

/// 左侧面板默认宽度（可拖拽调整）。
pub const SIDEBAR_WIDTH: f32 = 240.0;
/// 正文字号；同时决定 `rem` 基准（Root 每帧把 rem 设为该值）。
pub const FONT_BODY: gpui::Pixels = px(14.0);

fn color(hex: u32) -> Hsla {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

fn translucent(hex: u32, alpha: f32) -> Hsla {
    let mut value = color(hex);
    value.a = alpha;
    value
}

/// 应用浅色/深色两套配色。
pub fn apply(cx: &mut App) {
    let dark = cx.theme().mode.is_dark();
    let theme = Theme::global_mut(cx);
    if dark {
        // shadcn/ui Zinc 深色基准：与 Web 端（amux-web/src/index.css）同源换算成 sRGB
        theme.background = color(0x09090b);
        theme.foreground = color(0xfafafa);
        theme.border = color(0x27272a);
        theme.ring = color(0x71717b);
        theme.sidebar = color(0x17171a);
        theme.sidebar_foreground = color(0x9f9fa9);
        theme.sidebar_border = color(0x27272a);
        theme.sidebar_accent = color(0x27272a);
        theme.sidebar_accent_foreground = color(0xfafafa);
        theme.sidebar_primary = color(0xe4e4e7);
        theme.sidebar_primary_foreground = color(0x17171a);
        theme.list.active_highlight = true;
        theme.list_hover = color(0x212124);
        theme.list_active = color(0x27272a);
        theme.list_active_border = color(0x27272a);
        theme.list_even = color(0x101012);
        theme.list_head = color(0x101012);
        theme.muted = color(0x27272a);
        theme.muted_foreground = color(0x9f9fa9);
        theme.accent = color(0x27272a);
        theme.accent_foreground = color(0xfafafa);
        theme.primary = color(0xe4e4e7);
        theme.primary_hover = color(0xceced1);
        theme.primary_active = color(0xb8b8bb);
        theme.primary_foreground = color(0x17171a);
        theme.secondary = color(0x27272a);
        theme.secondary_hover = color(0x212124);
        theme.secondary_active = color(0x1b1b1e);
        theme.secondary_foreground = color(0xfafafa);
        theme.popover = color(0x18181b);
        theme.popover_foreground = color(0xfafafa);
        theme.group_box = color(0x18181b);
        theme.group_box_foreground = color(0xfafafa);
        theme.input = color(0x27272a);
        theme.selection = translucent(0xe4e4e7, 0.24);
        theme.caret = color(0xfafafa);
        theme.title_bar = color(0x17171a);
        theme.title_bar_border = color(0x17171a);
        theme.scrollbar_thumb = color(0x46464f);
        theme.scrollbar_thumb_hover = color(0x71717b);
        theme.link = color(0xfafafa);
        theme.danger = color(0xff6467);
        theme.danger_hover = color(0xff8588);
        theme.danger_active = color(0xff9fa1);
        theme.danger_foreground = color(0x09090b);
        theme.success = color(0x00a63e);
        theme.success_foreground = color(0xf2fcf5);
        theme.warning = color(0xfe9a00);
        theme.warning_foreground = color(0x1c1200);
        theme.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.6);
    } else {
        // shadcn/ui Zinc 浅色基准：与 Web 端（amux-web/src/index.css）同源，中性色强调
        theme.background = color(0xffffff);
        theme.foreground = color(0x09090b);
        theme.border = color(0xe4e4e7);
        theme.ring = color(0x9f9fa9);
        theme.sidebar = color(0xfafafa);
        theme.sidebar_foreground = color(0x71717b);
        theme.sidebar_border = color(0xe4e4e7);
        theme.sidebar_accent = color(0xf4f4f5);
        theme.sidebar_accent_foreground = color(0x18181b);
        theme.sidebar_primary = color(0x18181b);
        theme.sidebar_primary_foreground = color(0xfafafa);
        theme.list.active_highlight = true;
        theme.list_hover = color(0xf4f4f5);
        theme.list_active = color(0xe4e4e7);
        theme.list_active_border = color(0xe4e4e7);
        theme.list_even = color(0xfafafa);
        theme.list_head = color(0xfafafa);
        theme.muted = color(0xf4f4f5);
        theme.muted_foreground = color(0x71717b);
        theme.accent = color(0xf4f4f5);
        theme.accent_foreground = color(0x18181b);
        theme.primary = color(0x18181b);
        theme.primary_hover = color(0x2f2f32);
        theme.primary_active = color(0x464649);
        theme.primary_foreground = color(0xfafafa);
        theme.secondary = color(0xf4f4f5);
        theme.secondary_hover = color(0xf6f6f7);
        theme.secondary_active = color(0xf8f8f9);
        theme.secondary_foreground = color(0x18181b);
        theme.popover = color(0xffffff);
        theme.popover_foreground = color(0x09090b);
        theme.group_box = color(0xffffff);
        theme.group_box_foreground = color(0x09090b);
        theme.input = color(0xe4e4e7);
        theme.selection = translucent(0x18181b, 0.16);
        theme.caret = color(0x18181b);
        theme.title_bar = color(0xfafafa);
        theme.title_bar_border = color(0xfafafa);
        theme.scrollbar_thumb = color(0xd4d4d8);
        theme.scrollbar_thumb_hover = color(0x9f9fa9);
        theme.link = color(0x09090b);
        // 语义文字色需在浅色底上保持可读：这里是状态标签与行内校验文案的前景色
        theme.danger = color(0xe7000b);
        theme.danger_hover = color(0xfb2c36);
        theme.danger_active = color(0xff6467);
        theme.danger_foreground = color(0xffffff);
        theme.success = color(0x00a63e);
        theme.success_foreground = color(0xffffff);
        theme.warning = color(0xfe9a00);
        theme.warning_foreground = color(0x431407);
        theme.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.25);
    }
    theme.font_family = ".AppleSystemUIFont".into();
    theme.font_size = FONT_BODY;
    // 对齐 Web 端 shadcn New York 基准：radius-md 0.5rem、radius-lg 0.625rem（rem 基准 16px）
    theme.radius = px(8.0);
    theme.radius_lg = px(10.0);
    theme.shadow = true;
    // 组件读取的是由颜色派生的 tokens：改完颜色必须重建，否则组件仍用默认配色
    theme.tokens = ThemeTokens::from(&theme.colors);
}

/// 同步系统外观（`AMUX_THEME=dark|light` 可强制指定），随后套用配色令牌。
pub fn sync_appearance(window: Option<&mut Window>, cx: &mut App) {
    match std::env::var("AMUX_THEME").as_deref() {
        Ok("dark") => Theme::change(ThemeMode::Dark, window, cx),
        Ok("light") => Theme::change(ThemeMode::Light, window, cx),
        _ => Theme::sync_system_appearance(window, cx),
    }
    apply(cx);
}
