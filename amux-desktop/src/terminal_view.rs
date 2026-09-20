//! 终端视图：alacritty_terminal 的 VT 状态机 + 网格渲染（docs/DESIGN.md 桌面技术栈）。
//!
//! Server 只转发 PTY 原始字节流，本地在 `vte::ansi::Processor` 中解析进 `Term` 网格，
//! 光标定位、清屏、换行与颜色等控制序列由此被消化，不再原样上屏。输出经 SSE
//! 增量接收（docs/DESIGN.md「终端视图」），因此网格需与缓冲代际对齐：缓冲被重建
//! （切换终端、服务端丢弃旧输出）时网格整体重置。

use std::ops::Range;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use gpui::*;
use gpui_component::label::Label;
use gpui_component::{v_flex, ActiveTheme};

use crate::app::{AmuxApp, TerminalBackTab, TerminalTab};
use crate::state::{Core, TerminalBuffer};
use crate::ui;

/// 等宽字号与格子尺寸（Menlo 系 advance≈0.6em）；行列换算与网格行高共用。
const FONT_SIZE: f32 = 13.0;
pub const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
pub const CELL_HEIGHT: f32 = FONT_SIZE * 1.3;
/// 本地回滚历史行数（`Term` 一次性按总行数分配内存，取值需克制）。
const SCROLLBACK_LINES: usize = 5_000;
/// 新建网格时的初始行列（真实尺寸由画布实测后同步）。
const INITIAL_COLS: u16 = 100;
const INITIAL_ROWS: u16 = 30;
/// 行列上下限：过小的窗口不应把 PTY 压到不可用尺寸。
pub const MIN_COLS: u16 = 20;
pub const MAX_COLS: u16 = 500;
pub const MIN_ROWS: u16 = 5;
pub const MAX_ROWS: u16 = 200;

/// 终端渲染样式：跟随应用明暗主题。
struct TermStyle {
    dark: bool,
    bg: Hsla,
    mono_font: SharedString,
}

/// alacritty 事件汇（本视图不消费 term 事件，beep / title 等一律忽略）。
struct TermProxy;

impl EventListener for TermProxy {
    fn send_event(&self, _event: Event) {}
}

/// `Term::new` / `resize` 的行列描述（total_lines = 屏幕 + 回滚历史）。
struct TermDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermDims {
    fn columns(&self) -> usize {
        self.cols
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn total_lines(&self) -> usize {
        self.rows + SCROLLBACK_LINES
    }
}

/// 本地终端屏幕：VT 网格 + 已喂入的缓冲位置。
pub struct TerminalScreen {
    term: Term<TermProxy>,
    parser: Processor,
    /// 已喂入网格的缓冲代际与字节数（代际不同说明缓冲被重建，网格需重置）
    generation: u64,
    fed: usize,
    cols: u16,
    rows: u16,
    /// IME 组合中的文本（提交后整段发送，组合中不上屏）
    marked_text: Option<String>,
    /// 最近一次画布实测区（IME 候选窗锚定光标用）
    last_bounds: Option<Bounds<Pixels>>,
}

impl Default for TerminalScreen {
    fn default() -> Self {
        let dims = TermDims {
            cols: INITIAL_COLS as usize,
            rows: INITIAL_ROWS as usize,
        };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Default::default()
        };
        Self {
            term: Term::new(config, &dims, TermProxy),
            parser: Processor::new(),
            // 初始代为全新序号：网格尚未见过任何缓冲，首次同步必然按重建处理
            generation: crate::state::next_terminal_generation(),
            fed: 0,
            cols: INITIAL_COLS,
            rows: INITIAL_ROWS,
            marked_text: None,
            last_bounds: None,
        }
    }
}

impl TerminalScreen {
    /// 喂入缓冲中的新增输出；缓冲被重建时先重置网格。
    fn sync(&mut self, buffer: &TerminalBuffer) {
        if self.generation != buffer.generation() {
            self.reset(buffer.generation());
        }
        let bytes = buffer.bytes();
        if self.fed < bytes.len() {
            self.parser.advance(&mut self.term, &bytes[self.fed..]);
            self.fed = bytes.len();
        }
    }

    fn reset(&mut self, generation: u64) {
        let dims = TermDims {
            cols: self.cols as usize,
            rows: self.rows as usize,
        };
        self.term = Term::new(
            Config {
                scrolling_history: SCROLLBACK_LINES,
                ..Default::default()
            },
            &dims,
            TermProxy,
        );
        self.parser = Processor::new();
        self.generation = generation;
        self.fed = 0;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(TermDims {
            cols: cols as usize,
            rows: rows as usize,
        });
    }

    /// 回滚历史滚动（正数为向上进入历史）。
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// 终端是否处于 bracketed paste 模式（DECSET 2004）。
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// 光标所在格子的屏幕坐标（IME 候选窗锚定）。
    fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let bounds = self.last_bounds?;
        let content = self.term.renderable_content();
        let col = content.cursor.point.column.0 as f32 * CELL_WIDTH;
        let row = (content.cursor.point.line.0 + content.display_offset as i32).max(0) as f32
            * CELL_HEIGHT;
        Some(Bounds::new(
            bounds.origin + point(px(col), px(row)),
            size(px(CELL_WIDTH), px(CELL_HEIGHT)),
        ))
    }
}

/// 终端视图：网格 + 隐形测量画布（行列同步与 IME 注册都在绘制期完成）。
pub fn render(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    this.terminal.sync(&core.view.detail.terminal_output);
    let theme = ui::Colors::of(cx.theme());
    let style = TermStyle {
        dark: cx.theme().is_dark(),
        bg: theme.background,
        mono_font: theme.mono_font_family.clone(),
    };
    let rows = this.terminal.snapshot_rows(&style);
    let focus = this.terminal_focus.clone();

    let mut output =
        v_flex()
            .id("terminal-view")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(theme.background)
            .font_family(style.mono_font.clone())
            .text_size(px(FONT_SIZE))
            .track_focus(&focus)
            .key_context("Terminal")
            .on_click(cx.listener(|this, _, window, cx| window.focus(&this.terminal_focus, cx)))
            .on_action(cx.listener(|this, _: &TerminalTab, _, cx| {
                this.send_terminal_input(b"\t".to_vec(), cx)
            }))
            .on_action(cx.listener(|this, _: &TerminalBackTab, _, cx| {
                this.send_terminal_input(b"\x1b[Z".to_vec(), cx)
            }))
            .on_key_down(cx.listener(|this, event, _, cx| this.terminal_key_down(event, cx)))
            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.terminal_scroll(event, cx)))
            .children(rows);

    // 测量层：不绘制内容，只用绘制期的实际尺寸换算行列并同步给 PTY
    let weak = cx.entity().downgrade();
    output = output.child(
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                // IME：焦点在终端上时注册输入处理器（gpui 每帧清空，需每帧重注册）
                let focus = view.read(cx).terminal_focus.clone();
                window.handle_input(&focus, ElementInputHandler::new(bounds, view.clone()), cx);
                view.update(cx, |this, cx| {
                    this.terminal.last_bounds = Some(bounds);
                    this.sync_terminal_size(bounds.size.width, bounds.size.height, cx);
                });
            },
        )
        .absolute()
        .inset_0(),
    );

    let exited = core
        .view
        .detail
        .terminals
        .iter()
        .find(|terminal| Some(terminal.id.as_str()) == core.view.detail.active_terminal.as_deref())
        .is_some_and(|terminal| terminal.state == amux_common::api::TerminalState::Exited);
    if exited {
        output = output.child(
            div()
                .absolute()
                .bottom_2()
                .left_1_2()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.danger.opacity(0.15))
                .child(Label::new("进程已退出").text_xs().text_color(theme.danger)),
        );
    }
    output.into_any_element()
}

impl TerminalScreen {
    /// 可见网格 → 按行带样式的文本 run（相邻同款格子合并为一个 run）。
    fn snapshot_rows(&self, style: &TermStyle) -> Vec<AnyElement> {
        let content = self.term.renderable_content();
        let screen_lines = self.term.screen_lines();
        let offset = content.display_offset;
        let cursor = content.cursor;
        let cursor_line = cursor.point.line.0 + offset as i32;
        let cursor_col = cursor.point.column.0;
        let show_cursor = cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden;

        let mut rows: Vec<Vec<(char, CellStyle)>> = vec![Vec::new(); screen_lines];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset as i32;
            if row < 0 || row >= screen_lines as i32 {
                continue;
            }
            let cell = indexed.cell;
            // 宽字符占两列：跳过占位格，保留宽字符本体（等宽字体自然双宽）
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let is_cursor = show_cursor
                && indexed.point.line.0 == cursor_line
                && indexed.point.column.0 == cursor_col;
            rows[row as usize].push((
                cell.c,
                CellStyle::of(
                    cell.fg,
                    cell.bg,
                    cell.flags,
                    is_cursor,
                    content.colors,
                    style,
                ),
            ));
        }

        rows.into_iter()
            .map(|cells| render_row(cells, style))
            .collect()
    }
}

/// 单行格子 → 带样式文本。
fn render_row(cells: Vec<(char, CellStyle)>, style: &TermStyle) -> AnyElement {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let font = font(style.mono_font.clone());
    for (ch, cell) in cells {
        let run_font = Font {
            weight: if cell.bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if cell.italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
            ..font.clone()
        };
        // 相邻同款格子并成一个 run（len 为 utf8 字节数）
        if let Some(last) = runs.last_mut() {
            if last.font == run_font
                && last.color == cell.fg
                && last.background_color == Some(cell.bg)
                && last.underline.is_some() == cell.underline
            {
                last.len += ch.len_utf8();
                text.push(ch);
                continue;
            }
        }
        text.push(ch);
        runs.push(TextRun {
            len: ch.len_utf8(),
            font: run_font,
            color: cell.fg,
            background_color: Some(cell.bg),
            underline: cell.underline.then(UnderlineStyle::default),
            strikethrough: None,
        });
    }
    div()
        .w_full()
        .h(px(CELL_HEIGHT))
        .overflow_hidden()
        .text_size(px(FONT_SIZE))
        .child(StyledText::new(text).with_runs(runs))
        .into_any_element()
}

/// 一组格子的渲染样式（作为 run 合并的键）。
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Hsla,
    bg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl CellStyle {
    fn of(
        fg: Color,
        bg: Color,
        flags: Flags,
        is_cursor: bool,
        colors: &alacritty_terminal::term::color::Colors,
        style: &TermStyle,
    ) -> Self {
        let mut fg = color_to_hsla(fg, colors, style);
        let mut bg = color_to_hsla(bg, colors, style);
        if flags.contains(Flags::INVERSE) || is_cursor {
            std::mem::swap(&mut fg, &mut bg);
        }
        Self {
            fg,
            bg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
        }
    }
}

/// 终端色 → Hsla。默认前景/背景取应用主题 token（默认色盘仅兜底 ANSI 命名色）。
fn color_to_hsla(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    style: &TermStyle,
) -> Hsla {
    let rgb = match color {
        Color::Named(NamedColor::Foreground) => colors[NamedColor::Foreground]
            .unwrap_or_else(|| rgb_from_hex(if style.dark { 0xd4d4d4 } else { 0x383a42 })),
        Color::Named(NamedColor::Background) => {
            colors[NamedColor::Background].unwrap_or_else(|| rgb_from_hsla(style.bg))
        }
        Color::Named(name) => {
            let default = default_palette(name, style.dark);
            colors[name].unwrap_or(default)
        }
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => indexed_palette(i, colors, style.dark),
    };
    gpui::rgb(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32).into()
}

fn rgb_from_hex(hex: u32) -> Rgb {
    Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

/// 主题背景 Hsla → Rgb（把主题 token 喂给终端默认背景；gpui 无 Hsla→alacritty 的桥）。
fn rgb_from_hsla(color: Hsla) -> Rgb {
    let (r, g, b) = hsl_to_rgb(color.h, color.s, color.l);
    Rgb {
        r: (r * 255.0) as u8,
        g: (g * 255.0) as u8,
        b: (b * 255.0) as u8,
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |t: f32| -> f32 {
        let mut t = t;
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

/// 默认 16 色盘（近似 VS Code 明/暗两套，保证两主题下的可读性）。
const DARK_PALETTE: [(NamedColor, u32); 18] = [
    (NamedColor::Foreground, 0xd4d4d4),
    (NamedColor::Background, 0x161617),
    (NamedColor::Cursor, 0xd4d4d4),
    (NamedColor::Black, 0x2a2a2b),
    (NamedColor::Red, 0xcd3131),
    (NamedColor::Green, 0x0dbc79),
    (NamedColor::Yellow, 0xe5e510),
    (NamedColor::Blue, 0x2472c8),
    (NamedColor::Magenta, 0xbc3fbc),
    (NamedColor::Cyan, 0x11a8cd),
    (NamedColor::White, 0xe5e5e5),
    (NamedColor::BrightBlack, 0x767676),
    (NamedColor::BrightRed, 0xf14c4c),
    (NamedColor::BrightGreen, 0x23d18b),
    (NamedColor::BrightYellow, 0xf5f543),
    (NamedColor::BrightBlue, 0x3b8eea),
    (NamedColor::BrightMagenta, 0xd670d6),
    (NamedColor::BrightCyan, 0x29b8db),
];

const LIGHT_PALETTE: [(NamedColor, u32); 18] = [
    (NamedColor::Foreground, 0x383a42),
    (NamedColor::Background, 0xffffff),
    (NamedColor::Cursor, 0x383a42),
    (NamedColor::Black, 0x000000),
    (NamedColor::Red, 0xcd3131),
    (NamedColor::Green, 0x00bc00),
    (NamedColor::Yellow, 0x949800),
    (NamedColor::Blue, 0x0451a5),
    (NamedColor::Magenta, 0xbc05bc),
    (NamedColor::Cyan, 0x0598bc),
    (NamedColor::White, 0x555555),
    (NamedColor::BrightBlack, 0x666666),
    (NamedColor::BrightRed, 0xcd3131),
    (NamedColor::BrightGreen, 0x05bc79),
    (NamedColor::BrightYellow, 0x949800),
    (NamedColor::BrightBlue, 0x0451a5),
    (NamedColor::BrightMagenta, 0xbc05bc),
    (NamedColor::BrightCyan, 0x0598bc),
];

/// 16 色默认盘：按主题明暗取对应色表。
fn default_palette(name: NamedColor, dark: bool) -> Rgb {
    let table: &[(NamedColor, u32); 18] = if dark { &DARK_PALETTE } else { &LIGHT_PALETTE };
    for (candidate, hex) in table {
        if *candidate == name {
            return rgb_from_hex(*hex);
        }
    }
    // Dim 变体未单列：回退前景色
    rgb_from_hex(if dark { 0xd4d4d4 } else { 0x383a42 })
}

/// xterm 256 色标准盘（0-15 走 16 色默认盘）。
fn indexed_palette(i: u8, colors: &alacritty_terminal::term::color::Colors, dark: bool) -> Rgb {
    if let Some(rgb) = colors[i as usize] {
        return rgb;
    }
    match i {
        0..=15 => {
            let named = match i {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                _ => NamedColor::BrightWhite,
            };
            default_palette(named, dark)
        }
        16..=231 => {
            let i = i as u32 - 16;
            Rgb {
                r: ((i / 36) * 51) as u8,
                g: (((i % 36) / 6) * 51) as u8,
                b: ((i % 6) * 51) as u8,
            }
        }
        _ => {
            let gray = (8 + (i as u32 - 232) * 10) as u8;
            Rgb {
                r: gray,
                g: gray,
                b: gray,
            }
        }
    }
}

/// 修饰键合并为 xterm CSI 参数值（1+shift+2*alt+4*ctrl）。
fn csi_modifier(modifiers: &Modifiers) -> Option<u8> {
    let value =
        1 + (modifiers.shift as u8) + 2 * (modifiers.alt as u8) + 4 * (modifiers.control as u8);
    (value > 1).then_some(value)
}

/// 按键 → xterm 字节序列（tab / shift-tab 由 action 路径处理，走不到这里）。
pub fn keystroke_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    let key = keystroke.key.as_str();
    match key {
        "enter" => return Some(vec![b'\r']),
        "backspace" => return Some(vec![0x7f]),
        "escape" => return Some(b"\x1b".to_vec()),
        "left" | "right" | "up" | "down" | "home" | "end" => {
            let letter = match key {
                "left" => 'D',
                "right" => 'C',
                "up" => 'A',
                "down" => 'B',
                "home" => 'H',
                _ => 'F',
            };
            return Some(match csi_modifier(&modifiers) {
                Some(value) => format!("\x1b[1;{value}{letter}").into_bytes(),
                None => format!("\x1b[{letter}").into_bytes(),
            });
        }
        "delete" => {
            return Some(match csi_modifier(&modifiers) {
                Some(value) => format!("\x1b[3;{value}~").into_bytes(),
                None => b"\x1b[3~".to_vec(),
            })
        }
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        "space" => {
            if modifiers.control {
                return Some(vec![0x00]);
            }
        }
        "alt" | "control" | "shift" | "platform" | "function" | "capslock" => return None,
        _ => {}
    }

    if modifiers.control {
        // Ctrl+字母/数字 → 控制字符
        let base = key.bytes().next()?;
        if key.len() == 1 && base.is_ascii_graphic() {
            return Some(vec![base & 0x1f]);
        }
        return None;
    }
    // key_char 是实际产生的字符（区分大小写与符号布局）
    let ch = keystroke.key_char.as_ref()?.chars().next()?;
    let mut bytes = ch.to_string().into_bytes();
    if modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// IME 文本输入：中文/日文等组合输入经输入处理器注入。GPUI 平台层保证分发不重不漏
/// ——输入源激活时 printable 按键优先送输入处理器，否则走 `on_key_down` 字节路径。
/// 组合中的 marked text 不在网格内预览（由平台候选窗展示），提交后整段发送。
impl EntityInputHandler for AmuxApp {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // 终端无文本缓冲语义
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // 返回 marked text 末尾的空选区，给 IME 一个落点
        let len = self
            .terminal
            .marked_text
            .as_deref()
            .map(|text| text.encode_utf16().count())
            .unwrap_or(0);
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let len = self.terminal.marked_text.as_deref()?.encode_utf16().count();
        Some(0..len)
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.terminal.marked_text = None;
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // IME 提交（或平台直接插入）：整段作为输入字节发送
        self.terminal.marked_text = None;
        if !text.is_empty() {
            let bytes = text.as_bytes().to_vec();
            self.send_terminal_input(bytes, cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal.marked_text = Some(new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.terminal.cursor_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// 网格与输出缓冲的对齐逻辑（不涉及渲染）：控制序列被解析而非原样上屏，
/// 增量喂入与整体喂入等价，缓冲重建时旧内容被丢弃。
#[cfg(test)]
mod tests {
    // 只按需引入：`use super::*` 会把父模块 glob 进来的 `gpui::test` 属性宏
    // 一起带进来，遮蔽内置的 `#[test]` 并导致宏展开自递归。
    use alacritty_terminal::term::cell::Flags;

    use super::{TerminalBuffer, TerminalScreen};

    /// 可见网格文本（按行拼接并去掉行尾填空）。
    fn grid_text(screen: &TerminalScreen) -> String {
        let content = screen.term.renderable_content();
        let mut rows = String::new();
        let mut line = String::new();
        let mut current = 0;
        for indexed in content.display_iter {
            while current < indexed.point.line.0 {
                rows.push_str(line.trim_end());
                rows.push('\n');
                line.clear();
                current += 1;
            }
            if !indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                line.push(indexed.cell.c);
            }
        }
        rows.push_str(line.trim_end());
        rows.trim_end().to_string()
    }

    #[test]
    fn control_sequences_are_interpreted_instead_of_printed() {
        let mut buffer = TerminalBuffer::default();
        buffer.append(b"hello\x1b[2K\x1b[1;1Hhi\r\nthere");
        let mut screen = TerminalScreen::default();

        screen.sync(&buffer);

        assert_eq!(grid_text(&screen), "hi\nthere");
    }

    #[test]
    fn incremental_and_single_shot_feeds_agree() {
        let mut buffer = TerminalBuffer::default();
        buffer.append(b"abc");
        let mut screen = TerminalScreen::default();
        screen.sync(&buffer);

        buffer.append(b"\x1b[1;1HZ");
        screen.sync(&buffer);

        assert_eq!(grid_text(&screen), "Zbc");
    }

    #[test]
    fn rebuilt_buffer_discards_previous_grid() {
        let mut buffer = TerminalBuffer::default();
        buffer.append(b"old");
        let mut screen = TerminalScreen::default();
        screen.sync(&buffer);

        buffer.reset();
        buffer.append(b"new");
        screen.sync(&buffer);

        assert_eq!(grid_text(&screen), "new");
    }
}
