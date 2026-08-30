//! 终端视图：alacritty_terminal VT 状态机 + GPUI 网格渲染（docs/DESIGN.md「终端」）。
//!
//! 服务端只送 PTY 原始字节流，本地 `ansi::Processor` 解析进 `Term` 网格（即
//! 「终端历史由应用侧维护」）；渲染按行聚合格子为带背景色的文本 run。
//! 终端实体属于 MachineView（应用连接维度），输入经 `terminal.input` 上行。

use base64::Engine as _;
use gpui::*;
use gpui_component::{label::Label, v_flex};
use protocol::{OpResult, TerminalInputParams, TerminalResizeParams};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

const FONT_SIZE: f32 = 13.0;
/// 等宽字体的字符宽/行高近似（Menlo 系 advance≈0.6em）。仅用于行列换算，
/// 不影响格子渲染本身的对齐（对齐由文本布局保证）。
const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
const CELL_HEIGHT: f32 = FONT_SIZE * 1.3;
/// 滚动缓冲行数（应用侧历史；Term 网格一次性按总行数分配内存，取值需克制）
const SCROLLBACK_LINES: usize = 5_000;

const TERM_FONT_FAMILY: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(target_os = "windows") {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

/// alacritty 事件汇（本视图不消费 term 事件，beep/title 等一律忽略）。
struct TermProxy;

impl EventListener for TermProxy {
    fn send_event(&self, _event: Event) {}
}

/// `Term::new/resize` 的行列描述（total_lines = 屏幕 + 滚动缓冲）。
#[derive(Clone, Copy)]
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

/// MachineView 中的一条终端记录（连接维度；session_id 仅用于面板分组展示）。
#[derive(Clone)]
pub(crate) struct TerminalEntry {
    pub(crate) id: String,
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) view: Entity<TerminalState>,
}

pub(crate) struct TerminalState {
    pub(crate) id: String,
    term: Term<TermProxy>,
    parser: Processor,
    pub(crate) focus: FocusHandle,
    client: crate::ws::WsClient,
    /// shell 已退出（terminal.exit 通知；保持 UI 供查看残屏，由用户关闭）
    pub(crate) exited: bool,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

impl TerminalState {
    pub(crate) fn new(
        id: String,
        client: crate::ws::WsClient,
        cols: u16,
        rows: u16,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Default::default()
        };
        let term = Term::new(
            config,
            &TermDims {
                cols: cols as usize,
                rows: rows as usize,
            },
            TermProxy,
        );
        Self {
            id,
            term,
            parser: Processor::new(),
            focus: cx.focus_handle(),
            client,
            exited: false,
            cols,
            rows,
        }
    }

    /// 喂入 PTY 输出字节流。
    pub(crate) fn advance(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.parser.advance(&mut self.term, bytes);
        cx.notify();
    }

    fn send_input(&self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let id = self.id.clone();
        let client = self.client.clone();
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        cx.spawn(async move |_, _cx| {
            // 失败静默：断连时终端已被 server 释放，UI 随 disconnected 清理
            let _: Result<OpResult, _> = client
                .request(
                    protocol::method::TERMINAL_INPUT,
                    Some(TerminalInputParams { terminal_id: id, data }),
                )
                .await;
        })
        .detach();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(bytes) = keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        self.send_input(bytes, cx);
    }

    fn handle_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = (-event.delta.pixel_delta(px(CELL_HEIGHT)).y / px(CELL_HEIGHT)).round() as i32;
        if lines != 0 {
            // 滚轮向上（delta.y 为负）→ 正向 Delta 进入回滚历史
            self.term.scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    /// 画布实测尺寸 → 行列变化时同步 Term 网格并上报 terminal.resize。
    fn sync_size(&mut self, width: Pixels, height: Pixels, cx: &mut Context<Self>) {
        let cols = ((width.as_f32() / CELL_WIDTH).floor() as u16).clamp(2, 500);
        let rows = ((height.as_f32() / CELL_HEIGHT).floor() as u16).clamp(2, 200);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(TermDims {
            cols: cols as usize,
            rows: rows as usize,
        });
        let id = self.id.clone();
        let client = self.client.clone();
        cx.spawn(async move |_, _cx| {
            let _: Result<OpResult, _> = client
                .request(
                    protocol::method::TERMINAL_RESIZE,
                    Some(TerminalResizeParams {
                        terminal_id: id,
                        cols,
                        rows,
                    }),
                )
                .await;
        })
        .detach();
        cx.notify();
    }
}

impl Render for TerminalState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.snapshot_rows();
        let focus = self.focus.clone();
        let weak = cx.entity().downgrade();
        let mut container = v_flex()
            .id("terminal-view")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(rgb(0x161617))
            .track_focus(&focus)
            .on_click(cx.listener(|this, _ev, window, cx| {
                window.focus(&this.focus.clone(), cx);
            }))
            .on_key_down(cx.listener(Self::handle_key))
            .on_scroll_wheel(cx.listener(Self::handle_scroll))
            .children(rows);

        // 隐形画布：在 paint 阶段拿到真实 bounds 驱动行列同步
        let paint_weak = weak.clone();
        container = container.child(canvas(
            |_bounds, _window, _cx| (),
            move |canvas_bounds, (), _window, cx| {
                // 实体可能在 paint 前被丢弃（切换会话等），失败可安全忽略
                let _ = paint_weak.update(cx, |state, cx| {
                    // 画布随容器铺满，bounds 即终端可视区
                    state.sync_size(canvas_bounds.size.width, canvas_bounds.size.height, cx);
                });
            },
        ));
        let _ = weak;

        if self.exited {
            container = container.child(
                div()
                    .absolute()
                    .bottom_2()
                    .left_1_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(0x3a2a2a))
                    .child(
                        Label::new("进程已退出")
                            .text_xs()
                            .text_color(rgb(0xf0c0c0)),
                    ),
            );
        }
        container
    }
}

/// 可见网格 → 按行带样式文本 run。
impl TerminalState {
    fn snapshot_rows(&self) -> Vec<AnyElement> {
        let content = self.term.renderable_content();
        let screen_lines = self.term.screen_lines();
        let offset = content.display_offset;
        let cursor = content.cursor;
        let cursor_row = cursor.point.line.0 + offset as i32;
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
                && indexed.point.line.0 == cursor_row
                && indexed.point.column.0 == cursor_col;
            rows[row as usize].push((
                cell.c,
                CellStyle::of(cell.fg, cell.bg, cell.flags, is_cursor, content.colors),
            ));
        }

        rows.into_iter()
            .enumerate()
            .map(|(row_idx, cells)| self.render_row(row_idx, cells))
            .collect()
    }

    fn render_row(&self, _row_idx: usize, cells: Vec<(char, CellStyle)>) -> AnyElement {
        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let font = font(TERM_FONT_FAMILY);
        for (ch, style) in cells {
            let run_font = Font {
                weight: if style.bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                },
                style: if style.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
                ..font.clone()
            };
            // 相邻同款格子并成一个 run（len 为 utf8 字节数）
            if let Some(last) = runs.last_mut() {
                if last.font == run_font
                    && last.color == style.fg
                    && last.background_color == Some(style.bg)
                    && last.underline.is_some() == style.underline
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
                color: style.fg,
                background_color: Some(style.bg),
                underline: style.underline.then(UnderlineStyle::default),
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
    ) -> Self {
        let mut fg = color_to_hsla(fg, colors, true);
        let mut bg = color_to_hsla(bg, colors, false);
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

fn color_to_hsla(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    is_fg: bool,
) -> Hsla {
    let rgb = match color {
        Color::Named(name) => {
            let default = default_palette(name, is_fg);
            colors[name].unwrap_or(default)
        }
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => indexed_palette(i, colors),
    };
    gpui::rgb(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32).into()
}

/// 16 色默认盘（VS Code Dark+ 近似）。
fn default_palette(name: NamedColor, is_fg: bool) -> Rgb {
    use NamedColor::*;
    const TABLE: [(NamedColor, u32); 18] = [
        (Foreground, 0xd4d4d4),
        (Background, 0x161617),
        (Cursor, 0xd4d4d4),
        (Black, 0x2a2a2b),
        (Red, 0xcd3131),
        (Green, 0x0dbc79),
        (Yellow, 0xe5e510),
        (Blue, 0x2472c8),
        (Magenta, 0xbc3fbc),
        (Cyan, 0x11a8cd),
        (White, 0xe5e5e5),
        (BrightBlack, 0x767676),
        (BrightRed, 0xf14c4c),
        (BrightGreen, 0x23d18b),
        (BrightYellow, 0xf5f543),
        (BrightBlue, 0x3b8eea),
        (BrightMagenta, 0xd670d6),
        (BrightCyan, 0x29b8db),
    ];
    if name == BrightWhite {
        return Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        };
    }
    let _ = is_fg;
    for (candidate, hex) in TABLE {
        if candidate == name {
            return Rgb {
                r: (hex >> 16) as u8,
                g: (hex >> 8) as u8,
                b: hex as u8,
            };
        }
    }
    // Dim 变体未单列：回退前景色
    Rgb {
        r: 0xd4,
        g: 0xd4,
        b: 0xd4,
    }
}

/// xterm 256 色标准盘（0-15 走 16 色默认盘）。
fn indexed_palette(i: u8, colors: &alacritty_terminal::term::color::Colors) -> Rgb {
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
            default_palette(named, false)
        }
        16..=231 => {
            let i = i as u32 - 16;
            let r = ((i / 36) * 51) as u8;
            let g = (((i % 36) / 6) * 51) as u8;
            let b = ((i % 6) * 51) as u8;
            Rgb { r, g, b }
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
fn csi_modifier(m: &Modifiers) -> Option<u8> {
    let v = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.control as u8);
    (v > 1).then_some(v)
}

/// 按键 → xterm 字节序列（覆盖 shell/TUI 常用键；IME 组合输入暂不支持）。
fn keystroke_to_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let m = ks.modifiers;
    let key = ks.key.as_str();
    match key {
        "enter" => return Some(vec![b'\r']),
        "tab" => {
            return Some(if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            })
        }
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
            return Some(match csi_modifier(&m) {
                Some(v) => format!("\x1b[1;{v}{letter}").into_bytes(),
                None => format!("\x1b[{letter}").into_bytes(),
            });
        }
        "delete" => {
            return Some(match csi_modifier(&m) {
                Some(v) => format!("\x1b[3;{v}~").into_bytes(),
                None => b"\x1b[3~".to_vec(),
            })
        }
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        "space" => {
            if m.control {
                return Some(vec![0x00]);
            }
        }
        "alt" | "control" | "shift" | "platform" | "function" | "capslock" => return None,
        _ => {}
    }

    if m.control {
        // Ctrl+字母/数字 → 控制字符
        let base = key.bytes().next()?;
        if key.len() == 1 && base.is_ascii_graphic() {
            return Some(vec![base & 0x1f]);
        }
        return None;
    }
    // key_char 是实际产生的字符（区分大小写/符号布局）
    let ch = ks.key_char.as_ref()?.chars().next()?;
    let mut bytes = ch.to_string().into_bytes();
    if m.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}
