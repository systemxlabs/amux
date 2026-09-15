//! 终端视图：渲染 PTY 输出（简化文本网格）与控制键输入。
//!
//! 输出按游标增量拉取（docs/DESIGN.md「终端视图」）；渲染为等宽文本行，
//! 不解析光标定位等全屏控制序列。

use gpui::*;
use gpui_component::button::*;
use gpui_component::{h_flex, v_flex, Sizable};

use crate::app::{text_input, AmuxApp};
use crate::state::Core;

/// 渲染的最大行数（保留最近输出）。
const MAX_LINES: usize = 400;

pub fn render(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let text = String::from_utf8_lossy(&core.view.detail.terminal_output).to_string();
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    if lines.len() > MAX_LINES {
        lines = lines.split_off(lines.len() - MAX_LINES);
    }

    let mut body = v_flex()
        .id("terminal-output")
        .w_full()
        .max_h(px(280.0))
        .overflow_y_scroll()
        .bg(gpui::black().opacity(0.85))
        .rounded_md()
        .p_2()
        .font_family("monospace")
        .text_xs();
    for line in lines {
        body = body.child(div().child(if line.is_empty() {
            " ".to_string()
        } else {
            line
        }));
    }

    let keys = [
        ("Ctrl-C", &b"\x03"[..]),
        ("Esc", &b"\x1b"[..]),
        ("Tab", &b"\t"[..]),
        ("↑", &b"\x1b[A"[..]),
        ("↓", &b"\x1b[B"[..]),
        ("Enter", &b"\r"[..]),
    ];
    let mut key_row = h_flex().gap_1().flex_wrap();
    for (label, bytes) in keys {
        key_row = key_row.child(
            Button::new(format!("term-key-{label}"))
                .xsmall()
                .ghost()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send_terminal_key(bytes, cx);
                })),
        );
    }

    v_flex()
        .w_full()
        .gap_2()
        .child(body)
        .child(key_row)
        .child(
            h_flex()
                .gap_2()
                .child(div().flex_1().child(text_input(&this.terminal_input)))
                .child(
                    Button::new("term-send")
                        .xsmall()
                        .primary()
                        .label("发送")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.send_terminal_line(window, cx)),
                        ),
                ),
        )
        .into_any()
}
