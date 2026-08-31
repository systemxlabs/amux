//! 输入区与发送/取消按钮列的对齐回归测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 历史回归：输入区最小高度曾挂在包裹 Input 的容器 div 上，而 Input
//! （auto_grow 起步 3 行）自然高小于该最小值且不随容器拉伸，导致按钮列
//! （items_end 底对齐到容器）垂到可见输入框下沿之外。锁定不变量：
//! 1) 按钮列底沿与输入框容器底沿对齐；
//! 2) 输入框容器保持 ≥96px 最小高度（宽松粘贴/拖放命中区域）。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, IntoElement};

use amux_desktop::app::{AmuxApp, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};

#[gpui::test]
fn input_buttons_align_with_input_bottom(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = std::env::temp_dir().join(format!("amux-input-align-{}", std::process::id()));

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_dir));
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            let mut machine = MachineView::new(
                MachineConfig {
                    name: "test".into(),
                    url: "ws://127.0.0.1:9/".into(),
                    token: "t".into(),
                },
                cx,
            );
            machine.status = MachineStatus::Online;
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: 0,
                id: "session-1".into(),
            });
        });
    });

    // 渲染真实 AmuxApp 根视图（main_row → render_main → render_input）
    cx.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );

    let input = cx
        .debug_bounds("input-align-input")
        .expect("输入框容器应参与布局");
    let btn_col = cx
        .debug_bounds("input-align-btn-col")
        .expect("按钮列应参与布局");

    // 回归点 1：按钮列底沿与输入框容器底沿对齐（允许 1px 舍入误差）
    let delta = (input.bottom() - btn_col.bottom()).abs();
    assert!(
        delta <= px(1.),
        "发送/取消按钮列底沿（{:#?}）应与输入框底沿（{:#?}）对齐",
        btn_col.bottom(),
        input.bottom()
    );

    // 回归点 2：最小高度 96px 保持（宽松命中区域），且随窗口高度压缩不丢失
    assert!(
        input.size.height >= px(96.),
        "输入框容器最小高度 96px 不应丢失（实际 {:#?}）",
        input.size.height
    );
}
