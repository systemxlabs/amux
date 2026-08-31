//! 终端 Tab 键回归测试：焦点不得被祖先 context 的全局 tab 绑定抢走。
//!
//! 历史回归：GPUI 中 action 绑定先于 key_down 监听器派发，gpui-component
//! 的 Root 在顶层 context 全局绑定 tab（焦点循环）。终端未持更深 context
//! 的 tab 绑定时，按 Tab 会把焦点移出终端，此后键盘输入全部落到别的元素
//! 上，终端表现为"卡死"。锁定不变量：焦点在终端时按 Tab/Shift+Tab，
//! 终端 context 的绑定必须优先于祖先 context 的全局 tab 绑定，焦点留在终端。

use std::rc::Rc;

use gpui::{
    actions, div, px, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::{v_flex, ActiveTheme};

use crate::terminal::TerminalState;
use crate::ws::WsClient;

actions!(terminal_tab_test, [StealTabFocus]);

/// 终端 + 一个 tab stop 诱饵元素。Host 在祖先 context 绑定 tab 并把焦点
/// 移到诱饵——复刻 gpui-component Root 的全局焦点循环；终端回归（未持
/// 更深绑定）时，按 Tab 焦点会落到诱饵上。
struct TabFocusHost {
    terminal: Entity<TerminalState>,
    decoy: FocusHandle,
    stolen: Rc<std::cell::Cell<bool>>,
}

impl Render for TabFocusHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        v_flex()
            .key_context("Host")
            .size_full()
            .bg(bg)
            .on_action(cx.listener(|this, _: &StealTabFocus, window, cx| {
                this.stolen.set(true);
                window.focus(&this.decoy.clone(), cx);
            }))
            .child(self.terminal.clone())
            .child(
                div()
                    .id("tab-focus-decoy")
                    .track_focus(&self.decoy)
                    .h(px(20.))
                    .child("decoy"),
            )
    }
}

#[gpui::test]
fn tab_keeps_focus_in_terminal(cx: &mut gpui::TestAppContext) {
    // 祖先 context 的全局 tab 绑定（优先级低于终端 context 的绑定）
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::terminal::init(cx);
        cx.bind_keys([
            KeyBinding::new("tab", StealTabFocus, Some("Host")),
            KeyBinding::new("shift-tab", StealTabFocus, Some("Host")),
        ]);
    });

    let stolen = Rc::new(std::cell::Cell::new(false));
    let stolen_for_host = stolen.clone();
    let (host, cx) = cx.add_window_view(|window, cx| {
        // 连接目标不可达即可：send_input 静默失败，不影响焦点断言
        let client = WsClient::connect_with_token("ws://127.0.0.1:9/".into(), "t".into());
        let terminal = cx.new(|cx| TerminalState::new("tab-test".into(), client, 80, 24, cx));
        TabFocusHost {
            terminal,
            // 焦点循环只认 tab_stop 的 FocusHandle；decoy 模拟真实 UI 的按钮类元素
            decoy: cx.focus_handle().tab_stop(true),
            stolen: stolen_for_host,
        }
    });

    let terminal_focus = cx.update(|_, cx| host.read(cx).terminal.read(cx).focus.clone());
    cx.update(|window, cx| window.focus(&terminal_focus, cx));

    for keystroke in ["tab", "shift-tab"] {
        cx.simulate_keystrokes(keystroke);
        let still_focused = cx.update(|window, cx| {
            window
                .focused(cx)
                .map(|f| f == terminal_focus)
                .unwrap_or(false)
        });
        assert!(
            still_focused,
            "终端内按 {keystroke} 后焦点必须留在终端，不得被全局焦点循环抢走"
        );
        assert!(
            !stolen.get(),
            "按 {keystroke} 不应触发祖先 context 的全局 tab 绑定"
        );
    }
}
