//! 登录页面：无法成功连接 Server 时进入，内容居中（docs/PRD.md「登录页面」）。

use gpui::*;
use gpui_component::alert::Alert;
use gpui_component::button::*;
use gpui_component::input::Input;
use gpui_component::label::Label;
use gpui_component::{v_flex, ActiveTheme, Disableable as _};

use crate::app::AmuxApp;
use crate::state::{ConnectionStatus, Core};
use crate::ui;

/// 登录页面：错误提示 + Server 地址 / token + 进入按钮。
pub fn render(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let can_enter = !this.server_input.read(cx).value().trim().is_empty()
        && !this.token_input.read(cx).value().trim().is_empty();

    let mut card = v_flex()
        .w_full()
        .max_w(rems(26.))
        .gap_3()
        .p_4()
        .bg(theme.popover)
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .child(
            Label::new("连接 Server")
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground),
        );
    // 错误提示只在连接失败时出现：本地无连接信息时没有可展示的错误（docs/PRD.md「登录页面」）
    if let ConnectionStatus::Failed(error) = &core.status {
        card = card.child(Alert::error("login-error", error.clone()).title("连接失败"));
    }
    let card = card
        .child(field(
            "Server 地址",
            Input::new(&this.server_input).w_full().into_any_element(),
            &theme,
        ))
        .child(field(
            "认证 token",
            Input::new(&this.token_input).w_full().into_any_element(),
            &theme,
        ))
        .child(
            Button::new("login-enter")
                .primary()
                .w_full()
                .label("进入")
                .loading(matches!(core.status, ConnectionStatus::Connecting))
                .disabled(!can_enter)
                .on_click(cx.listener(|this, _, window, cx| this.login(window, cx))),
        );

    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_3()
        .child(card)
        .into_any_element()
}

/// 表单字段：灰标签 + 控件。
fn field(label: &str, control: AnyElement, theme: &ui::Colors) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .items_start()
        .child(
            Label::new(label.to_string())
                .text_sm()
                .text_color(theme.muted_foreground),
        )
        .child(control)
        .into_any_element()
}
