//! 通用弹窗：确认弹窗、结果提示与表单弹窗。
//!
//! 表单字段复用调用方持有的输入框实体（打开弹窗前由调用方填入初值），弹窗内容
//! 每帧重建，因此输入内容无需另外同步。

use std::rc::Rc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputState};
use gpui_component::label::Label;
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable, WindowExt};
use gpui_component::notification::Notification;

use crate::app::AmuxApp;

const CANCEL_TEXT: &str = "取消";

/// 表单弹窗的目标：新增，或编辑既有条目（按名称定位）。
#[derive(Debug, Clone)]
pub enum FormTarget {
    New,
    Edit(String),
}

/// 确认弹窗：点击确认后执行 `on_ok`。
pub fn confirm(
    window: &mut Window,
    cx: &mut Context<AmuxApp>,
    title: &'static str,
    description: String,
    ok_text: &'static str,
    ok_variant: ButtonVariant,
    on_ok: impl Fn(&mut AmuxApp, &mut Context<AmuxApp>) + 'static,
) {
    let app = cx.entity();
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = Rc::clone(&on_ok);
        let app = app.clone();
        alert
            .button_props(
                DialogButtonProps::default()
                    .ok_text(ok_text)
                    .ok_variant(ok_variant)
                    .cancel_text(CANCEL_TEXT)
                    .show_cancel(true),
            )
            .title(title)
            .description(description.clone())
            .on_ok(move |_, _, cx| {
                app.update(cx, |this, cx| on_ok(this, cx));
                true
            })
            .on_cancel(|_, _, _| true)
    });
}

/// 结果提示弹窗（保存成功/失败等）：只有「确定」按钮。
pub fn alert(
    window: &mut Window,
    cx: &mut Context<AmuxApp>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) {
    let title = title.into();
    let description = description.into();
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert
            .button_props(
                DialogButtonProps::default()
                    .ok_text("确定")
                    .show_cancel(false),
            )
            .title(title.clone())
            .description(description.clone())
            .on_ok(|_, _, _| true)
    });
}

/// 表单弹窗：`fields` 为（字段名，输入框实体）列表，宽度以 rem 给出。
///
/// 保存按钮点击后执行 `on_save`，返回 false 时保留弹窗（校验未通过）。
/// 手写页脚：库的 `Dialog` 不渲染 `button_props` 的确定/取消按钮（只有
/// `AlertDialog` 渲染），与 `on_save` 的返回值联动也只能自绘。
pub fn form(
    window: &mut Window,
    cx: &mut Context<AmuxApp>,
    title: &'static str,
    ok_label: &'static str,
    width_rems: f32,
    fields: Vec<(&'static str, Entity<InputState>)>,
    on_save: impl Fn(&mut AmuxApp, &mut Context<AmuxApp>) -> bool + 'static,
) {
    let app = cx.entity();
    let on_save = Rc::new(on_save);
    let width = rems(width_rems).to_pixels(window.rem_size());
    let muted_foreground = cx.theme().muted_foreground;
    window.open_dialog(cx, move |dialog, _, _| {
        let save = Rc::clone(&on_save);
        let save_app = app.clone();
        let ok_app = app.clone();
        let ok_save = Rc::clone(&on_save);
        let fields = fields.clone();
        dialog
            .title(title)
            .width(width)
            .content(move |content, _, _| {
                let mut body = v_flex().gap_2();
                for (label, input) in &fields {
                    body = body
                        .child(Label::new(*label).text_sm().text_color(muted_foreground))
                        .child(Input::new(input).w_full());
                }
                content.child(body)
            })
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("form-dialog-cancel")
                            .small()
                            .label(CANCEL_TEXT)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("form-dialog-ok").small().primary().label(ok_label).on_click(
                        move |_, window, cx| {
                            if save_app.update(cx, |this, cx| save(this, cx)) {
                                window.close_dialog(cx);
                            }
                        },
                    )),
            )
            // 回车确认（Esc 由 Dialog 自身处理）
            .on_ok(move |_, _, cx| ok_app.update(cx, |this, cx| ok_save(this, cx)))
    });
}

/// 后台任务排队的提示 → 通知对象。
pub fn note_notification(note: crate::state::Note) -> Notification {
    use crate::state::NoteLevel;
    let notification = match note.level {
        NoteLevel::Success => Notification::success(note.message),
        NoteLevel::Warning => Notification::warning(note.message),
        NoteLevel::Error => Notification::error(note.message),
    };
    notification.title("amux")
}
