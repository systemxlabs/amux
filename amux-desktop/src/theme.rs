//! 暖色系视觉令牌：覆盖 gpui-component 的语义主题（浅色/深色两套），
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
        theme.background = color(0x242422);
        theme.foreground = color(0xf0efed);
        theme.border = color(0x353431);
        theme.ring = color(0x4c8dff);
        theme.sidebar = color(0x1b1b1a);
        theme.sidebar_foreground = color(0xd3d2ce);
        theme.sidebar_border = color(0x292927);
        theme.sidebar_accent = color(0x343432);
        theme.sidebar_accent_foreground = color(0xf5f4f1);
        theme.sidebar_primary = color(0x4c8dff);
        theme.sidebar_primary_foreground = color(0xffffff);
        theme.list.active_highlight = true;
        theme.list_hover = color(0x2a2a28);
        theme.list_active = color(0x303b4c);
        theme.list_active_border = color(0x303b4c);
        theme.list_even = color(0x20201f);
        theme.list_head = color(0x20201f);
        theme.muted = color(0x323230);
        theme.muted_foreground = color(0xa9a8a2);
        theme.accent = color(0x383836);
        theme.accent_foreground = color(0xf4f3f0);
        theme.primary = color(0x4c8dff);
        theme.primary_hover = color(0x5e99ff);
        theme.primary_active = color(0x3d7ef0);
        theme.primary_foreground = color(0xffffff);
        theme.secondary = color(0x30302e);
        theme.secondary_hover = color(0x3a3a37);
        theme.secondary_active = color(0x41413d);
        theme.secondary_foreground = color(0xecebe8);
        theme.popover = color(0x2c2c2a);
        theme.popover_foreground = color(0xf0efed);
        theme.group_box = color(0x2c2c2a);
        theme.group_box_foreground = color(0xf0efed);
        theme.input = color(0x44423e);
        theme.selection = translucent(0x4c8dff, 0.32);
        theme.caret = color(0x4c8dff);
        theme.title_bar = color(0x1b1b1a);
        theme.title_bar_border = color(0x1b1b1a);
        theme.scrollbar_thumb = color(0x3e3e3b);
        theme.scrollbar_thumb_hover = color(0x4a4a47);
        theme.link = color(0x6ea3ff);
        theme.danger = color(0xff6b60);
        theme.danger_hover = color(0xff7b72);
        theme.danger_active = color(0xee5d54);
        theme.danger_foreground = color(0xffffff);
        theme.success = color(0x56c789);
        theme.success_foreground = color(0x10271b);
        theme.warning = color(0xd9a741);
        theme.warning_foreground = color(0x2a2105);
        theme.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.45);
    } else {
        theme.background = color(0xf1f1ef);
        theme.foreground = color(0x1d1d1f);
        theme.border = color(0xe3e3df);
        theme.ring = color(0x0a84ff);
        theme.sidebar = color(0xededea);
        theme.sidebar_foreground = color(0x41413d);
        theme.sidebar_border = color(0xe1e1dd);
        theme.sidebar_accent = color(0xdededa);
        theme.sidebar_accent_foreground = color(0x1d1d1f);
        theme.sidebar_primary = color(0x0a84ff);
        theme.sidebar_primary_foreground = color(0xffffff);
        theme.list.active_highlight = true;
        theme.list_hover = color(0xededea);
        theme.list_active = color(0xe3ebf6);
        theme.list_active_border = color(0xe3ebf6);
        theme.list_even = color(0xf7f7f5);
        theme.list_head = color(0xf7f7f5);
        theme.muted = color(0xe8e8e5);
        theme.muted_foreground = color(0x686761);
        theme.accent = color(0xe7e7e3);
        theme.accent_foreground = color(0x1d1d1f);
        theme.primary = color(0x0a84ff);
        theme.primary_hover = color(0x268fff);
        theme.primary_active = color(0x0071e3);
        theme.primary_foreground = color(0xffffff);
        theme.secondary = color(0xe8e8e5);
        theme.secondary_hover = color(0xdededa);
        theme.secondary_active = color(0xd4d4d0);
        theme.secondary_foreground = color(0x2b2b2d);
        theme.popover = color(0xfdfdfc);
        theme.popover_foreground = color(0x1d1d1f);
        theme.group_box = color(0xfdfdfc);
        theme.group_box_foreground = color(0x1d1d1f);
        theme.input = color(0xdadad6);
        theme.selection = translucent(0x0a7aff, 0.22);
        theme.caret = color(0x0a84ff);
        theme.title_bar = color(0xededea);
        theme.title_bar_border = color(0xededea);
        theme.scrollbar_thumb = color(0xd2d2cf);
        theme.scrollbar_thumb_hover = color(0xbfbfbc);
        theme.link = color(0x0a84ff);
        // 语义文字色需在浅色底上保持可读：这里是状态标签与行内校验文案的前景色
        theme.danger = color(0xc53030);
        theme.danger_hover = color(0xb4232b);
        theme.danger_active = color(0xa61b1b);
        theme.danger_foreground = color(0xffffff);
        theme.success = color(0x18794e);
        theme.success_foreground = color(0xffffff);
        theme.warning = color(0xa16207);
        theme.warning_foreground = color(0x6b4f00);
        theme.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.25);
    }
    theme.font_family = ".AppleSystemUIFont".into();
    theme.font_size = FONT_BODY;
    theme.radius = px(8.0);
    theme.radius_lg = px(12.0);
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
