//! 终端视图：alacritty_terminal VT 状态机 + GPUI 网格渲染。
//!
//! 服务端只送 PTY 原始字节流，本地 `ansi::Processor` 解析进 `Term` 网格（即
//! 「终端历史由应用侧维护」）；渲染按行聚合格子为带背景色的文本 run。
//! 终端实体属于 MachineView（应用连接维度），输入经 `terminal.input` 上行。

use std::ops::Range;

use base64::Engine as _;
use gpui::*;
use gpui_component::{label::Label, v_flex, ActiveTheme};
use protocol::{OpResult, TerminalInputParams, TerminalResizeParams};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

const FONT_SIZE: f32 = 13.0;
/// 等宽字体的字符宽/行高近似（Menlo 系 advance≈0.6em）。仅用于行列换算，
/// 不影响格子渲染本身的对齐（对齐由文本布局保证）。
const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
const CELL_HEIGHT: f32 = FONT_SIZE * 1.3;
/// 滚动缓冲行数（应用侧历史；Term 网格一次性按总行数分配内存，取值需克制）
const SCROLLBACK_LINES: usize = 5_000;

/// 终端渲染的主题样式：跟随应用明暗主题。
struct TermThemeStyle {
    dark: bool,
    bg: Hsla,
    mono_font: SharedString,
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

// Tab/Shift+Tab 进终端输入：GPUI 中 action 绑定先于 key_down 监听器派发，
// gpui-component 的 Root 又全局绑定了 tab（焦点循环），终端不持更深的绑定
// 就会被抢焦点、整体失联（表现为"卡死"）。
actions!(terminal, [Tab, BackTab]);

/// 注册终端按键绑定（app 启动与测试共用）。
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", Tab, Some("Terminal")),
        KeyBinding::new("shift-tab", BackTab, Some("Terminal")),
    ]);
}

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
    /// IME 组合中的文本（拼音等；提交前不上屏，由平台候选窗展示）
    marked_text: Option<String>,
    /// 最近一次画布实测区（IME 候选窗锚定光标用）
    last_bounds: Option<Bounds<Pixels>>,
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
            marked_text: None,
            last_bounds: None,
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
                    Some(TerminalInputParams {
                        terminal_id: id,
                        data,
                    }),
                )
                .await;
        })
        .detach();
    }

    /// Tab/Shift+Tab 走 action 路径（见 actions! 说明），发送对应字节序列。
    fn on_tab(&mut self, _: &Tab, _window: &mut Window, cx: &mut Context<Self>) {
        self.send_input(vec![b'\t'], cx);
    }

    fn on_back_tab(&mut self, _: &BackTab, _window: &mut Window, cx: &mut Context<Self>) {
        self.send_input(b"\x1b[Z".to_vec(), cx);
    }

    fn handle_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // 粘贴：macOS Cmd+V / 其他平台 Ctrl+V（剪贴板是 IME 之外输入中文的主要途径）
        let m = &event.keystroke.modifiers;
        let paste_modifier = if cfg!(target_os = "macos") {
            m.platform
        } else {
            m.control
        };
        if paste_modifier && event.keystroke.key == "v" {
            cx.stop_propagation();
            self.paste(cx);
            return;
        }
        let Some(bytes) = keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        self.send_input(bytes, cx);
    }

    /// 粘贴剪贴板文本：终端处于 bracketed paste 模式（应用开启 DECSET 2004）时
    /// 原样包裹发送，多行粘贴不会被逐行当作回车执行；否则换行归一为 \r。
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let bytes = if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        };
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
    /// 布局未就绪时 bounds 可能退化为 0：直接忽略，绝不能把 0 除行高 clamp 成
    /// 最小值——那会把 PTY 缩到 2 行，输出全部丢失（曾因此"看不到输出"）。
    fn sync_size(&mut self, width: Pixels, height: Pixels, cx: &mut Context<Self>) {
        if width < px(CELL_WIDTH) || height < px(CELL_HEIGHT) {
            return;
        }
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
        // 终端配色跟随应用主题，避免终端内容与周围面板形成不一致的明暗层级。
        let theme = cx.theme().clone();
        let style = TermThemeStyle {
            dark: theme.is_dark(),
            bg: theme.background,
            mono_font: theme.mono_font_family.clone(),
        };
        let rows = self.snapshot_rows(&style);
        let focus = self.focus.clone();
        let weak = cx.entity().downgrade();
        let mut container = v_flex()
            .id("terminal-view")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(style.bg)
            .track_focus(&focus)
            .key_context("Terminal")
            .on_click(cx.listener(|this, _ev, window, cx| {
                window.focus(&this.focus.clone(), cx);
            }))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_back_tab))
            .on_key_down(cx.listener(Self::handle_key))
            .on_scroll_wheel(cx.listener(Self::handle_scroll))
            .children(rows);

        // 隐形画布：绝对定位铺满容器（默认样式尺寸为 0，曾把 resize 退化成 2 行），
        // 在 paint 阶段拿到真实 bounds 驱动行列同步 + 注册 IME 输入处理器
        let paint_weak = weak.clone();
        container = container.child(
            canvas(
                |_bounds, _window, _cx| (),
                move |canvas_bounds, (), window, cx| {
                    // 实体可能在 paint 前被丢弃（切换会话等），失败可安全忽略
                    let _ = paint_weak.update(cx, |state, cx| {
                        state.last_bounds = Some(canvas_bounds);
                        // 画布随容器铺满，bounds 即终端可视区
                        state.sync_size(canvas_bounds.size.width, canvas_bounds.size.height, cx);
                    });
                    // IME：焦点在终端上时注册输入处理器（每帧重注册，gpui 每帧清空）
                    if let Some(view) = paint_weak.upgrade() {
                        let focus = view.read(cx).focus.clone();
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(canvas_bounds, view),
                            cx,
                        );
                    }
                },
            )
            .absolute()
            .inset_0(),
        );
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
                    .bg(theme.danger.opacity(0.15))
                    .child(Label::new("进程已退出").text_xs().text_color(theme.danger)),
            );
        }
        container
    }
}

/// 可见网格 → 按行带样式文本 run。
impl TerminalState {
    fn snapshot_rows(&self, style: &TermThemeStyle) -> Vec<AnyElement> {
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
            .enumerate()
            .map(|(row_idx, cells)| self.render_row(row_idx, cells, style))
            .collect()
    }

    fn render_row(
        &self,
        _row_idx: usize,
        cells: Vec<(char, CellStyle)>,
        style: &TermThemeStyle,
    ) -> AnyElement {
        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let font = font(style.mono_font.clone());
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
        style: &TermThemeStyle,
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

/// 终端色 → Hsla。默认前景/背景取应用主题 token（默认盘仅兜底 ANSI 命名色）。
fn color_to_hsla(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    style: &TermThemeStyle,
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

/// 主题背景 Hsla → Rgb（用于把主题 token 喂给终端默认背景）。
fn rgb_from_hsla(color: Hsla) -> Rgb {
    // 手写 HSL→RGB：gpui 的 Hsla 无到 alacritty Rgb 的桥
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

/// IME 文本输入（docs 限制外的补充能力）：中文/日文等组合输入经
/// `EntityInputHandler` 注入。GPUI 平台层保证分发不重不漏——IME 输入源激活时
/// printable 按键优先送输入处理器，否则走 `on_key_down` 字节路径。
/// 组合中的 marked text 不在网格内预览（由平台候选窗展示），提交后整段发送。
impl EntityInputHandler for TerminalState {
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
            .marked_text
            .as_deref()
            .map(|t| t.encode_utf16().count())
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
        let len = self.marked_text.as_deref()?.encode_utf16().count();
        Some(0..len)
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_text = None;
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // IME 提交（或平台直接插入）：整段作为输入字节发送
        self.marked_text = None;
        if !text.is_empty() {
            self.send_input(text.as_bytes().to_vec(), cx);
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
        self.marked_text = Some(new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 候选窗锚定到光标所在格子
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

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// 16 色默认盘：按主题明暗取对应色表。
fn default_palette(name: NamedColor, dark: bool) -> Rgb {
    let table: &[(NamedColor, u32); 18] = if dark { &DARK_PALETTE } else { &LIGHT_PALETTE };
    for (candidate, hex) in table {
        if *candidate == name {
            return Rgb {
                r: (*hex >> 16) as u8,
                g: (*hex >> 8) as u8,
                b: *hex as u8,
            };
        }
    }
    // Dim 变体未单列：回退前景色
    let fallback = if dark { 0xd4d4d4 } else { 0x383a42 };
    Rgb {
        r: (fallback >> 16) as u8,
        g: (fallback >> 8) as u8,
        b: fallback as u8,
    }
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
        // tab/shift-tab 由 action 路径处理（绑定先于 key_down 派发，走不到这里）
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
