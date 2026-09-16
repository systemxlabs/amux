//! 通用弹窗：确认弹窗与表单弹窗。
//!
//! 表单字段复用调用方持有的输入框实体（打开弹窗前由调用方填入初值），
//! 弹窗内容每帧重建，因此输入内容无需另外同步。

use std::rc::Rc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::dialog::{DialogButtonProps, DialogFooter};
use gpui_component::input::{Input, InputState};
use gpui_component::label::Label;
use gpui_component::{v_flex, WindowExt};

use crate::app::AmuxApp;

const CANCEL_TEXT: &str = "取消";

/// 表单弹窗的目标：新增，或编辑既有条目（按名称定位）。
#[derive(Debug, Clone)]
pub enum FormTarget {
    New,
    Edit(String),
}

/// 确认弹窗：点击确认按钮后执行 `on_ok`。
pub fn confirm(
    window: &mut Window,
    cx: &mut Context<AmuxApp>,
    title: String,
    description: String,
    ok_text: &'static str,
    danger: bool,
    on_ok: impl Fn(&mut AmuxApp, &mut Context<AmuxApp>) + 'static,
) {
    let app = cx.entity();
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = Rc::clone(&on_ok);
        let app = app.clone();
        let mut props = DialogButtonProps::default()
            .ok_text(ok_text)
            .cancel_text(CANCEL_TEXT)
            .show_cancel(true);
        if danger {
            props = props.ok_variant(ButtonVariant::Danger);
        }
        alert
            .title(title.clone())
            .description(description.clone())
            .button_props(props)
            .on_ok(move |_, _, cx| {
                app.update(cx, |this, cx| on_ok(this, cx));
                true
            })
    });
}

/// 表单弹窗：`fields` 为（字段名，输入框实体）列表。
///
/// 点击保存按钮后执行 `on_save`，返回 false 时保留弹窗（如校验未通过）。
pub fn form(
    window: &mut Window,
    cx: &mut Context<AmuxApp>,
    title: String,
    fields: Vec<(&'static str, Entity<InputState>)>,
    on_save: impl Fn(&mut AmuxApp, &mut Context<AmuxApp>) -> bool + 'static,
) {
    let app = cx.entity();
    let on_save = Rc::new(on_save);
    window.open_dialog(cx, move |dialog, _, _| {
        let save = Rc::clone(&on_save);
        let save_app = app.clone();
        let ok_app = app.clone();
        let ok_save = Rc::clone(&on_save);
        let mut body = v_flex().gap_3();
        for (label, input) in &fields {
            body = body
                .child(Label::new(*label))
                .child(Input::new(input).w_full());
        }
        dialog
            .title(title.clone())
            .child(body)
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("form-cancel")
                            .outline()
                            .label(CANCEL_TEXT)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("form-save").primary().label("保存").on_click(
                        move |_, window, cx| {
                            if save_app.update(cx, |this, cx| save(this, cx)) {
                                window.close_dialog(cx);
                            }
                        },
                    )),
            )
            // 回车确认（Esc 由 Dialog 的 keyboard 处理）
            .on_ok(move |_, _, cx| ok_app.update(cx, |this, cx| ok_save(this, cx)))
    });
}
