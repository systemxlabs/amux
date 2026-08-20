//! Wake-inspired visual tokens for the desktop workbench.
//!
//! The app keeps the product's three-panel layout, but uses a quieter macOS
//! palette: warm surfaces, a single system-blue accent, and semantic colors.

use gpui::{px, App, Hsla, Rgba, Window};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};

pub const SIDEBAR_WIDTH: f32 = 240.0;
pub const FONT_BODY: gpui::Pixels = px(14.0);
pub const SPACE_SM: gpui::Pixels = px(8.0);
pub const SPACE_MD: gpui::Pixels = px(12.0);

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

/// Apply Wake's warm light/dark surfaces over gpui-component's semantic tokens.
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
        theme.sidebar = color(0xedeDEA);
        theme.sidebar_foreground = color(0x41413d);
        theme.sidebar_border = color(0xe1e1dd);
        theme.sidebar_accent = color(0xdededa);
        theme.sidebar_accent_foreground = color(0x1d1d1f);
        theme.sidebar_primary = color(0x0a84ff);
        theme.sidebar_primary_foreground = color(0xffffff);
        theme.list.active_highlight = true;
        theme.list_hover = color(0xedeDEA);
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
        theme.title_bar = color(0xedeDEA);
        theme.title_bar_border = color(0xedeDEA);
        theme.scrollbar_thumb = color(0xd2d2cf);
        theme.scrollbar_thumb_hover = color(0xbfbfbc);
        theme.link = color(0x0a84ff);
        theme.danger = color(0xe5484d);
        theme.danger_hover = color(0xee5c61);
        theme.danger_active = color(0xd93c42);
        theme.danger_foreground = color(0xffffff);
        theme.success = color(0x2f9e63);
        theme.success_foreground = color(0xffffff);
        theme.warning = color(0xa16207);
        theme.warning_foreground = color(0xffffff);
        theme.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.25);
    }
    theme.font_family = ".AppleSystemUIFont".into();
    theme.font_size = FONT_BODY;
    theme.radius = px(8.0);
    theme.radius_lg = px(12.0);
    theme.shadow = true;
}

pub fn sync_appearance(window: Option<&mut Window>, cx: &mut App) {
    match std::env::var("AMUX_THEME").as_deref() {
        Ok("dark") => Theme::change(ThemeMode::Dark, window, cx),
        Ok("light") => Theme::change(ThemeMode::Light, window, cx),
        _ => Theme::sync_system_appearance(window, cx),
    }
    apply(cx);
}
