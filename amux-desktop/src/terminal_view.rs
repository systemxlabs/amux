//! 终端视图：渲染 PTY 输出（简化文本网格）与控制键输入。
//!
//! 输出按游标增量拉取（docs/DESIGN.md「终端视图」）；渲染为等宽文本行，
//! 不解析光标定位等全屏控制序列（字符网格与颜色由 daemon 侧输出原样呈现）。

use gpui::*;
use gpui_component::button::*;
use gpui_component::label::Label;
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable};

use crate::app::AmuxApp;
use crate::state::Core;
use crate::ui;

/// 等宽字号与行高（与终端行列换算一致的近似值）。
const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = FONT_SIZE * 1.3;
/// 等宽字符宽度近似（Menlo 系 advance≈0.6em），仅用于行列换算。
const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
/// 行列上下限：过小的窗口不应把 PTY 压到不可用尺寸。
const MIN_COLS: u16 = 20;
const MAX_COLS: u16 = 500;
const MIN_ROWS: u16 = 5;
const MAX_ROWS: u16 = 200;
/// 渲染的最大行数（保留最近输出）。
const MAX_LINES: usize = 400;

pub fn render(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let text = String::from_utf8_lossy(&core.view.detail.terminal_output).to_string();
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    let truncated = lines.len() > MAX_LINES;
    if truncated {
        lines = lines.split_off(lines.len() - MAX_LINES);
    }

    let exited = core
        .view
        .detail
        .terminals
        .iter()
        .find(|terminal| {
            Some(terminal.id.as_str()) == core.view.detail.active_terminal.as_deref()
        })
        .is_some_and(|terminal| terminal.state == amux_common::api::TerminalState::Exited);

    let mut output = v_flex()
        .id("terminal-view")
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .p_2()
        .overflow_y_scroll()
        .bg(theme.background)
        .font_family(theme.mono_font_family.clone())
        .text_size(px(FONT_SIZE));
    for line in lines {
        output = output.child(
            div()
                .w_full()
                .h(px(LINE_HEIGHT))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(theme.foreground)
                .child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line
                }),
        );
    }
    if core.view.detail.terminal_output.is_empty() {
        output = output.child(
            Label::new("（暂无输出）")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    // 测量层：不绘制内容，只用绘制期的实际尺寸换算行列并同步给 PTY
    let weak = cx.entity().downgrade();
    output = output.child(
        canvas(
            |_, _, _| (),
            move |bounds, (), _, cx| {
                let cols = (bounds.size.width.as_f32() / CELL_WIDTH).floor();
                let rows = (bounds.size.height.as_f32() / LINE_HEIGHT).floor();
                let cols = (cols as u16).clamp(MIN_COLS, MAX_COLS);
                let rows = (rows as u16).clamp(MIN_ROWS, MAX_ROWS);
                let _ = weak.update(cx, |this, cx| this.sync_terminal_size(cols, rows, cx));
            },
        )
        .absolute()
        .inset_0(),
    );
    if exited {
        output = output.child(
            div()
                .absolute()
                .bottom_2()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.danger.opacity(0.15))
                .child(
                    Label::new("进程已退出")
                        .text_xs()
                        .text_color(theme.danger),
                ),
        );
    }

    let keys = [
        ("Ctrl-C", &b"\x03"[..]),
        ("Esc", &b"\x1b"[..]),
        ("Tab", &b"\t"[..]),
        ("↑", &b"\x1b[A"[..]),
        ("↓", &b"\x1b[B"[..]),
        ("Enter", &b"\r"[..]),
    ];
    let mut key_row = h_flex().gap_1().flex_wrap().px_3().py_2();
    for (label, bytes) in keys {
        key_row = key_row.child(
            Button::new(SharedString::from(format!("term-key-{label}")))
                .xsmall()
                .ghost()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send_terminal_key(bytes, cx);
                })),
        );
    }

    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(output)
        .child(key_row)
        .child(
            h_flex().gap_2().px_3().pb_3().child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(this.terminal_input.clone()),
            ),
        )
        .into_any_element()
}
