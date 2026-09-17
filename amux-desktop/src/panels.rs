//! 左侧面板（会话列表）、中间面板右缘的悬浮按钮栏、右侧面板（工作目录 / 文件改动 /
//! 会话详情 / 会话活动 / 会话计划 / 终端）。

use amux_common::api::{Session, Terminal, TerminalState, Workflow};
use amux_common::domain::{Activity, GitDiffFile, GitDiffLineKind, SessionState};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::*;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::Input;
use gpui_component::label::Label;
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::scroll::{ScrollableElement as _, Scrollbar};
use gpui_component::spinner::Spinner;
use gpui_component::text::TextView;
use gpui_component::{
    h_flex, v_flex, ActiveTheme, Disableable as _, Icon, IconName, IconNamed, Selectable, Sizable,
};

use crate::app::AmuxApp;
use crate::diff;
use crate::difftree::{self, DiffNode};
use crate::sessions;
use crate::state::{Core, ListEntry, OpenTarget, SidePanel, WorkspaceNode};
use crate::terminal_view;
use crate::ui;

/// 改动审查视图左侧文件树宽度。
const DIFF_TREE_WIDTH: f32 = 168.0;
/// 工作目录树宽度。
const WORKSPACE_TREE_WIDTH: f32 = 196.0;
/// 工作目录树中每层缩进。
const TREE_INDENT: f32 = 14.0;

/// 改动面板图标：文件 diff（文件轮廓内含 +/−）。gpui-component 默认图标集无
/// 对应图标，SVG 由应用自有资产提供（main.rs `AmuxAssets`）。
struct FileDiffIcon;

impl IconNamed for FileDiffIcon {
    fn path(self) -> SharedString {
        "icons/file-diff.svg".into()
    }
}

// ---------- 左侧面板 ----------

/// 左侧面板内容：标题与新建入口、会话列表、设置入口。
pub fn render_sidebar(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let sidebar = theme.sidebar;
    let sidebar_border = theme.sidebar_border;
    let foreground = theme.foreground;

    let mut list = v_flex()
        .id("sidebar-sessions")
        .flex_1()
        .min_h_0()
        .track_scroll(&this.list_scroll)
        .overflow_y_scroll()
        .gap_1();
    for entry in core.entries.clone() {
        match &entry {
            ListEntry::Session(session) => {
                list = list.child(session_row(session, false, core, this, cx));
            }
            ListEntry::Workflow(workflow) => {
                list = list.child(workflow_row(workflow, core, this, cx));
            }
        }
    }
    if core.entries.is_empty() {
        list = list.child(
            Label::new("暂无会话")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }

    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .gap_2()
        .p_3()
        .bg(sidebar)
        .border_r_1()
        .border_color(sidebar_border)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new("会话列表")
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(foreground),
                )
                .child(div().flex_1())
                .child(
                    Button::new("goto-new-session")
                        .small()
                        .icon(IconName::Plus)
                        .tooltip("新会话 / 工作流")
                        .on_click(cx.listener(|this, _, _, cx| this.open_new_session(cx))),
                ),
        )
        .child(list)
        .child(
            h_flex().child(
                Button::new("open-settings")
                    .small()
                    .ghost()
                    .label("设置")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_settings(None, window, cx)),
                    ),
            ),
        )
        .into_any_element()
}

/// 普通会话行：状态图标、标题、活跃时间、工作中转圈；右键菜单重命名/删除。
///
/// `linked` 表示这是工作流会话下挂的关联普通会话，行首改用缩进符标记。
fn session_row(
    session: &Session,
    linked: bool,
    core: &Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let id = session.id.clone();
    let title = session_title(&session.title, &session.workspace);
    if this.renaming_id.as_deref() == Some(id.as_str()) {
        return rename_form(&id, this, cx);
    }
    let selected = matches!(&core.open, Some(OpenTarget::Session(open)) if open == &id);
    let theme = ui::Colors::of(cx.theme());
    let entry = ListEntry::Session(session.clone());

    let marker = if linked {
        Label::new("↳")
            .text_sm()
            .text_color(theme.muted_foreground)
            .into_any_element()
    } else {
        Icon::new(IconName::SquareTerminal)
            .small()
            .text_color(if selected {
                theme.primary
            } else {
                theme.muted_foreground
            })
            .into_any_element()
    };

    let row = h_flex()
        .w_full()
        .h_8()
        .px_1()
        .gap_1p5()
        .items_center()
        .child(marker)
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .items_center()
                .child(Label::new(title).text_sm().flex_1().min_w_0().truncate()),
        )
        .when(session.updated_at > 0, |row| {
            row.child(
                Label::new(ui::format_local_time(
                    session.updated_at,
                    ui::TimePrecision::Compact,
                ))
                .text_xs()
                .flex_none()
                .text_color(theme.muted_foreground),
            )
        })
        .child(busy_indicator(session.state, theme.primary));

    list_row(row, &id, selected, Some(entry), cx).into_any_element()
}

/// 工作流会话行：标题 + 展开开关 + 关联普通会话（展开时按自身活跃排序）。
fn workflow_row(
    workflow: &Workflow,
    core: &Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let id = workflow.id.clone();
    let title = if workflow.title.trim().is_empty() {
        "未命名工作流".to_string()
    } else {
        workflow.title.clone()
    };
    if this.renaming_id.as_deref() == Some(id.as_str()) {
        return rename_form(&id, this, cx);
    }
    let selected = matches!(&core.open, Some(OpenTarget::Workflow(open)) if open == &id);
    let expanded = core.expanded_workflows.contains(&id);
    let theme = ui::Colors::of(cx.theme());
    let entry = ListEntry::Workflow(workflow.clone());

    let header = h_flex()
        .w_full()
        .h_8()
        .px_1()
        .gap_1()
        .items_center()
        .child(
            Icon::new(IconName::Network)
                .small()
                .text_color(if selected {
                    theme.primary
                } else {
                    theme.muted_foreground
                }),
        )
        .child(Label::new(title).text_sm().flex_1().min_w_0().truncate())
        .when(workflow.updated_at > 0, |row| {
            row.child(
                Label::new(ui::format_local_time(
                    workflow.updated_at,
                    ui::TimePrecision::Compact,
                ))
                .text_xs()
                .flex_none()
                .text_color(theme.muted_foreground),
            )
        })
        .child(busy_indicator(workflow.state, theme.primary))
        .child(
            Button::new(format!("wf-toggle-{id}"))
                .xsmall()
                .ghost()
                .icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .tooltip("展开/折叠关联会话")
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        this.with_core(|core| {
                            if !core.expanded_workflows.remove(&id) {
                                core.expanded_workflows.insert(id.clone());
                            }
                        });
                        cx.notify();
                    }
                })),
        );

    let mut children = v_flex().gap_1();
    if expanded {
        for linked in &workflow.linked_sessions {
            children = children.child(session_row(linked, true, core, this, cx));
        }
    }

    v_flex()
        .gap_1()
        .p_1()
        .child(list_row(header, &id, selected, Some(entry), cx))
        .child(children)
        .into_any_element()
}

/// 行内重命名表单：输入框 + 保存/取消。
fn rename_form(id: &str, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    v_flex()
        .gap_1()
        .p_1()
        // 必须用 Input 组件渲染：直接挂 InputState 实体只会渲染成不可交互的文本
        .child(Input::new(&this.rename_input).w_full())
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(format!("rename-save-{id}"))
                        .small()
                        .primary()
                        .flex_1()
                        .label("保存")
                        .on_click(cx.listener(|this, _, _, cx| this.commit_rename(cx))),
                )
                .child(
                    Button::new(format!("rename-cancel-{id}"))
                        .small()
                        .ghost()
                        .flex_1()
                        .label("取消")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_rename(cx))),
                ),
        )
        .into_any_element()
}

/// 会话行外壳：选中/悬停样式 + 右键菜单（重命名 / 删除，docs/PRD.md 会话列表视图）。
fn list_row(
    row: impl IntoElement,
    id: &str,
    selected: bool,
    entry: Option<ListEntry>,
    cx: &mut Context<AmuxApp>,
) -> impl IntoElement {
    let theme = ui::Colors::of(cx.theme());
    let active = theme.list_active;
    let active_border = theme.list_active_border;
    let hover = theme.list_hover;
    let row_id = format!("sess-row-{id}");
    let open_id = id.to_string();
    let entry = entry.clone();
    let app = cx.entity();
    div()
        .id(SharedString::from(row_id))
        .relative()
        .w_full()
        .rounded_md()
        .bg(active.opacity(if selected { 1.0 } else { 0.0 }))
        .when(selected, |row| row.border_1().border_color(active_border))
        .hover(move |row| row.bg(hover))
        .on_click(cx.listener(move |this, _, _, cx| this.open_entry(&open_id, cx)))
        .context_menu(move |menu, _, _| {
            let Some(entry) = &entry else {
                return menu;
            };
            let rename_id = entry.id().to_string();
            let rename_app = app.clone();
            let delete_entry = entry.clone();
            let delete_app = app.clone();
            menu.item(PopupMenuItem::new("重命名").on_click(move |_, window, cx| {
                let rename_id = rename_id.clone();
                rename_app.update(cx, |this, cx| this.begin_rename(&rename_id, window, cx));
            }))
            .item(
                PopupMenuItem::new("删除会话").on_click(move |_, window, cx| {
                    delete_app.update(cx, |this, cx| {
                        this.confirm_delete(delete_entry.clone(), window, cx)
                    });
                }),
            )
        })
        .child(row)
}

/// 会话标题：未命名时回退到工作目录短名，避免空行难以辨识。
fn session_title(title: &str, workspace: &str) -> String {
    if !title.trim().is_empty() {
        return title.to_string();
    }
    if workspace.trim().is_empty() {
        return "未命名会话".to_string();
    }
    format!("（未命名）{}", ui::short_cwd(workspace))
}

/// 工作中转圈；空闲用等宽占位保持列宽稳定。
fn busy_indicator(state: SessionState, color: Hsla) -> AnyElement {
    if sessions::is_busy(state) {
        Spinner::new().xsmall().color(color).into_any_element()
    } else {
        div().size_2().into_any_element()
    }
}

// ---------- 悬浮按钮栏 ----------

/// 中间面板右缘的竖排悬浮按钮：点击展开对应的右侧面板。
pub fn render_rail(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut rail = v_flex()
        .gap_0p5()
        .p_1()
        .justify_center()
        .bg(theme.popover)
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .shadow_sm()
        // 点击按钮不应被下方对话区的点击处理捕获
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    let is_workflow = core.is_workflow();
    for panel in SidePanel::for_session(is_workflow) {
        rail = rail.child(rail_button(panel, core.side_panel == Some(panel), this, cx));
    }
    rail.into_any_element()
}

fn rail_button(
    panel: SidePanel,
    active: bool,
    _this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let label = panel.short_label();
    let icon: Icon = match panel {
        SidePanel::Workspace => IconName::FolderOpen.into(),
        SidePanel::Diff => FileDiffIcon.into(),
        SidePanel::Detail => IconName::Info.into(),
        SidePanel::Activities => IconName::Inbox.into(),
        SidePanel::Plan => IconName::Map.into(),
        SidePanel::Terminal => IconName::SquareTerminal.into(),
    };
    let color = if active {
        cx.theme().primary
    } else {
        cx.theme().muted_foreground
    };
    Button::new(format!("rail-{}", panel.label()))
        .small()
        .ghost()
        .icon(icon.text_color(color))
        .selected(active)
        .tooltip(label)
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_side_panel(panel, cx)))
        .into_any_element()
}

// ---------- 右侧面板 ----------

/// 右侧面板外壳与内容。
pub fn render_panel(
    core: &Core,
    panel: SidePanel,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    v_flex()
        .w_full()
        .h_full()
        .min_w_0()
        .bg(theme.popover)
        .border_l_1()
        .border_color(theme.border)
        .child(panel_header(panel, cx))
        .child(match panel {
            SidePanel::Workspace => workspace_panel(core, this, cx),
            SidePanel::Diff => diff_review(core, this, cx),
            SidePanel::Detail => detail_panel(core, cx),
            SidePanel::Activities => activities_panel(core, this, cx),
            SidePanel::Plan => plan_panel(core, this, cx),
            SidePanel::Terminal => terminal_panel(core, this, cx),
        })
        .into_any_element()
}

/// 面板标题栏（标题 + 关闭按钮）；改动面板与工作目录面板的工具栏由各自视图渲染。
fn panel_header(panel: SidePanel, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let header = h_flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .child(
            Label::new(match panel {
                SidePanel::Activities => "会话活动历史",
                SidePanel::Diff => "改动审查",
                other => other.label(),
            })
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.foreground),
        )
        .child(div().flex_1());
    header
        .child(
            Button::new("close-panel")
                .small()
                .ghost()
                .icon(IconName::Close)
                .tooltip("关闭面板")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_core(|core| core.side_panel = None);
                    cx.notify();
                })),
        )
        .into_any_element()
}

/// 工作目录面板：左侧文件树 + 右侧文件内容。
fn workspace_panel(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let root = core
        .view
        .session
        .as_ref()
        .map(|session| (session.machine.clone(), session.root_dir().to_string()));
    let mut tree = v_flex()
        .id("workspace-tree")
        .w(px(WORKSPACE_TREE_WIDTH))
        .h_full()
        .min_h_0()
        .gap_1()
        .p_1()
        .bg(theme.muted.opacity(0.35))
        .rounded_md()
        .overflow_y_scroll();
    match &root {
        Some((machine, _)) => {
            tree = tree.children(workspace_nodes(
                &core.view.detail.workspace_tree,
                machine,
                0,
                this,
                cx,
            ));
        }
        None => {
            tree = tree.child(
                Label::new("未选择会话")
                    .text_sm()
                    .text_color(theme.muted_foreground),
            );
        }
    }

    // 文件内容：不再重复展示文件路径（左侧树里选中项已能看出是哪个文件）
    let mut content = v_flex().flex_1().min_w_0().h_full().overflow_y_scrollbar();
    match &core.view.detail.file_content {
        // 文本文件内容以围栏块渲染（等宽、保留空白）
        Some(text) => {
            content = content.child(
                TextView::markdown("workspace-file-content", format!("```text\n{text}\n```"))
                    .selectable(true),
            );
        }
        None => {
            content = content.child(
                Label::new("在左侧选择文件查看内容")
                    .text_sm()
                    .text_color(theme.muted_foreground),
            );
        }
    }

    // 工具栏（docs/PRD.md「工作目录视图」）：折叠/展开文件树按钮左对齐，折叠/展开内容
    // 区域按钮右对齐
    let toolbar = h_flex()
        .items_center()
        .gap_2()
        .px_3()
        .pb_2()
        .child(
            Button::new("workspace-toggle-tree")
                .small()
                .ghost()
                .icon(if this.workspace_tree_visible {
                    IconName::PanelLeftClose
                } else {
                    IconName::PanelLeftOpen
                })
                .tooltip(if this.workspace_tree_visible {
                    "折叠文件树"
                } else {
                    "展开文件树"
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_workspace_tree(cx))),
        )
        .child(div().flex_1())
        .child(
            Button::new("workspace-toggle-content")
                .small()
                .ghost()
                .icon(if this.workspace_content_visible {
                    IconName::PanelRightClose
                } else {
                    IconName::PanelRightOpen
                })
                .tooltip(if this.workspace_content_visible {
                    "折叠内容"
                } else {
                    "展开内容"
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_workspace_content(cx))),
        );

    let mut body = h_flex().flex_1().min_h_0().gap_2().px_3().pb_3();
    if this.workspace_tree_visible {
        body = body.child(tree);
    }
    if this.workspace_content_visible {
        body = body.child(content);
    }
    v_flex()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(toolbar)
        .child(body)
        .into_any_element()
}

/// 工作目录树行：目录可折叠/展开（首次展开拉取子目录），文件可查看内容。
fn workspace_nodes(
    nodes: &[WorkspaceNode],
    machine: &str,
    depth: usize,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> Vec<AnyElement> {
    let theme = ui::Colors::of(cx.theme());
    let mut rows = Vec::new();
    for node in nodes {
        let entry = &node.entry;
        let indent = rems(0.5 + depth as f32 * (TREE_INDENT / 16.0));
        if entry.is_dir {
            rows.push(
                Button::new(SharedString::from(format!(
                    "workspace-entry-{}",
                    entry.path
                )))
                .small()
                .ghost()
                .w_full()
                .on_click(cx.listener({
                    let path = entry.path.clone();
                    move |this, _, _, cx| this.toggle_workspace_dir(path.clone(), cx)
                }))
                .child(
                    h_flex()
                        .w_full()
                        .justify_start()
                        .gap_1p5()
                        .pl(indent)
                        .child(
                            Icon::new(if node.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall()
                            .flex_none()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            Label::new(entry.name.clone())
                                .text_sm()
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        ),
                )
                .into_any_element(),
            );
            if node.expanded {
                match &node.children {
                    Some(children) => {
                        rows.extend(workspace_nodes(children, machine, depth + 1, this, cx))
                    }
                    None => rows.push(
                        Label::new("加载中…")
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .into_any_element(),
                    ),
                }
            }
            continue;
        }
        let selected = this.workspace_file.as_deref() == Some(entry.path.as_str());
        rows.push(
            Button::new(SharedString::from(format!("workspace-file-{}", entry.path)))
                .small()
                .ghost()
                .w_full()
                .selected(selected)
                .on_click(cx.listener({
                    let machine = machine.to_string();
                    let path = entry.path.clone();
                    move |this, _, _, cx| this.read_file(machine.clone(), path.clone(), cx)
                }))
                .child(
                    h_flex()
                        .w_full()
                        .justify_start()
                        .gap_1p5()
                        .pl(indent)
                        .child(Icon::new(IconName::File).xsmall().flex_none().text_color(
                            if selected {
                                theme.primary
                            } else {
                                theme.muted_foreground
                            },
                        ))
                        .child(
                            Label::new(entry.name.clone())
                                .text_sm()
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        ),
                )
                .into_any_element(),
        );
    }
    rows
}

/// 会话详情面板：会话元信息（工作流会话额外展示关联普通会话）。
fn detail_panel(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut body = v_flex()
        .flex_1()
        .min_h_0()
        .gap_2()
        .p_3()
        .overflow_y_scrollbar();
    if let Some(session) = &core.view.session {
        body = body
            .child(ui::info_row("会话 ID", &session.id, &theme))
            .child(ui::info_row("Agent", &session.agent, &theme))
            .child(ui::info_row("工作目录", &session.workspace, &theme));
        if !session.worktree_dir.is_empty() {
            body = body.child(ui::info_row("worktree", &session.worktree_dir, &theme));
        }
        body = body
            .child(ui::info_row(
                "状态",
                if sessions::is_busy(session.state) {
                    "工作中"
                } else {
                    "空闲"
                },
                &theme,
            ))
            .children(
                ui::context_usage_text(
                    core.view.detail.context_size,
                    core.view.detail.context_window_size,
                )
                .map(|text| ui::info_row("上下文", &text, &theme)),
            )
            .child(ui::info_row(
                "创建时间",
                &ui::format_local_time(session.created_at, ui::TimePrecision::Seconds),
                &theme,
            ))
            .child(ui::info_row("机器", &session.machine, &theme))
            .child(ui::info_row(
                "最近活跃",
                &ui::format_local_time(session.updated_at, ui::TimePrecision::Seconds),
                &theme,
            ));
    } else if let Some(workflow) = &core.view.workflow {
        body = body
            .child(ui::info_row("工作流 ID", &workflow.id, &theme))
            .child(ui::info_row(
                "状态",
                if sessions::is_busy(workflow.state) {
                    "工作中"
                } else {
                    "空闲"
                },
                &theme,
            ))
            .child(ui::info_row(
                "创建时间",
                &ui::format_local_time(workflow.created_at, ui::TimePrecision::Seconds),
                &theme,
            ))
            .child(ui::info_row(
                "最近活跃",
                &ui::format_local_time(workflow.updated_at, ui::TimePrecision::Seconds),
                &theme,
            ))
            .child(ui::info_row("计划", &workflow.plan, &theme))
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(Label::new("关联普通会话").font_weight(FontWeight::SEMIBOLD))
                            .child(
                                Label::new(format!("{}", workflow.linked_sessions.len()))
                                    .text_xs()
                                    .text_color(theme.muted_foreground),
                            ),
                    )
                    .children(workflow.linked_sessions.iter().map(|linked| {
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .items_center()
                            .child(
                                Icon::new(IconName::SquareTerminal)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(
                                Label::new(if linked.title.trim().is_empty() {
                                    linked.id.clone()
                                } else {
                                    linked.title.clone()
                                })
                                .text_sm()
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                            )
                            .child(
                                Label::new(linked.machine.clone())
                                    .text_xs()
                                    .flex_none()
                                    .text_color(theme.muted_foreground),
                            )
                    })),
            );
    } else {
        body = body.child(
            Label::new("未选择会话")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    body.into_any_element()
}

/// 会话活动面板：条目默认折叠为一行，点击展开详情；滚动分页见 docs/DESIGN.md「活动列表滚动机制」。
fn activities_panel(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut rows = v_flex()
        .id("activities-panel")
        .w_full()
        .flex_1()
        .min_h_0()
        .gap_2()
        .p_3()
        .pt_0()
        .track_scroll(&this.activities_scroll)
        .overflow_y_scroll();
    for activity in &core.view.detail.activities {
        rows = rows.child(activity_row(activity, this, cx));
    }
    if core.view.detail.activities.is_empty() {
        rows = rows.child(
            Label::new("暂无活动")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    rows.into_any_element()
}

/// 活动卡片：时间 + 种类 + 详情（折叠时单行截断）。
fn activity_row(activity: &Activity, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let timestamp = ui::activity_timestamp(activity);
    let key = format!("{timestamp}-{}", ui::activity_kind_detail(activity).0);
    let expanded = this.expanded_activities.contains(&key);
    let (kind, detail) = ui::activity_kind_detail(activity);
    let toggle_key = key.clone();
    div()
        .id(SharedString::from(format!("activity-{key}")))
        .w_full()
        .p_2()
        .bg(theme.muted.opacity(0.55))
        .rounded_md()
        .cursor_pointer()
        .hover(|card| card.bg(theme.muted))
        .on_click(cx.listener(move |this, _, _, cx| {
            if !this.expanded_activities.remove(&toggle_key) {
                this.expanded_activities.insert(toggle_key.clone());
            }
            cx.notify();
        }))
        .child(
            h_flex()
                .w_full()
                .gap_1p5()
                .items_start()
                .child(
                    Label::new(ui::format_local_time(timestamp, ui::TimePrecision::Seconds))
                        .text_xs()
                        .flex_none()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .flex_none()
                    .text_color(theme.muted_foreground),
                )
                .child(
                    Label::new(kind)
                        .text_xs()
                        .flex_none()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Label::new(if expanded {
                        detail.clone()
                    } else {
                        ui::one_line(&detail)
                    })
                    .text_sm()
                    .flex_1()
                    .min_w_0()
                    .when(!expanded, |label| label.truncate()),
                ),
        )
        .into_any_element()
}

/// 会话计划面板：`✓ / ● / ○` 标记 + 计划内容（无计划则空白）。
fn plan_panel(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut rows = v_flex().gap_1();
    for entry in &core.view.detail.plan {
        let (glyph, color) = match entry.status {
            amux_common::domain::SessionPlanStatus::Completed => ("✓", theme.muted_foreground),
            amux_common::domain::SessionPlanStatus::InProgress => ("●", theme.primary),
            amux_common::domain::SessionPlanStatus::Pending => ("○", theme.muted_foreground),
        };
        let done = entry.status == amux_common::domain::SessionPlanStatus::Completed;
        rows = rows.child(
            h_flex()
                .items_start()
                .gap_1p5()
                .py_0p5()
                .child(Label::new(glyph).text_sm().text_color(color).w(px(14.0)))
                .child(
                    Label::new(entry.content.clone())
                        .text_sm()
                        .when(done, |label| label.text_color(theme.muted_foreground))
                        .flex_1(),
                ),
        );
    }
    div()
        .id("plan-panel")
        .w_full()
        .flex_1()
        .min_h_0()
        .p_3()
        .pt_0()
        .child(
            v_flex()
                .id("plan-scroll")
                .w_full()
                .h_full()
                .track_scroll(&this.plan_scroll)
                .overflow_y_scroll()
                .child(rows),
        )
        .into_any_element()
}

/// 终端面板：标签栏（可切换/关闭/新建）+ 终端视图。
fn terminal_panel(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let session = core.view.session.as_ref();
    let online = session.is_some();
    let active = core.view.detail.active_terminal.clone();

    let mut tabs = h_flex().flex_wrap().gap_1().px_3().pb_2();
    for terminal in &core.view.detail.terminals {
        tabs = tabs.child(terminal_tab(terminal, active.as_deref(), cx));
    }
    tabs = tabs.child(
        Button::new("terminal-new")
            .small()
            .ghost()
            .icon(IconName::Plus)
            .tooltip("新建终端")
            .disabled(!online)
            .on_click(cx.listener(|this, _, _, cx| this.new_terminal(cx))),
    );

    let body = if core.view.detail.terminals.is_empty() {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .child(
                Label::new(if online {
                    "暂无终端，点击 + 新建"
                } else {
                    "未选择会话"
                })
                .text_sm()
                .text_color(theme.muted_foreground),
            )
            .into_any_element()
    } else {
        terminal_view::render(core, this, cx)
    };

    v_flex()
        .flex_1()
        .min_h_0()
        .child(tabs)
        .child(body)
        .into_any_element()
}

fn terminal_tab(
    terminal: &Terminal,
    active: Option<&str>,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let id = terminal.id.clone();
    let selected = active == Some(id.as_str());
    let title = format!(
        "{} {}",
        ui::truncate(&id, 12),
        match terminal.state {
            TerminalState::Running => "运行中",
            TerminalState::Exited => "已退出",
        }
    );
    h_flex()
        .id(SharedString::from(format!("terminal-tab-{id}")))
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(if selected {
            theme.list_active
        } else {
            theme.muted.opacity(0.35)
        })
        .cursor_pointer()
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, window, cx| {
                this.select_terminal(id.clone(), cx);
                // 切到某终端即把键盘交给它，键盘输入才落到 VT 网格
                window.focus(&this.terminal_focus, cx);
            }
        }))
        .child(Label::new(title).text_xs().max_w_24().truncate())
        .child(
            Button::new(SharedString::from(format!("terminal-tab-close-{id}")))
                .xsmall()
                .ghost()
                .icon(IconName::Close)
                .tooltip("关闭终端")
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| this.close_terminal(id.clone(), cx)
                })),
        )
        .into_any_element()
}

// ---------- 改动审查视图 ----------

/// 改动审查视图：工具栏 + 文件树 + inline 改动 + 选中后引用到会话输入框。
fn diff_review(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let files = core
        .view
        .detail
        .diff
        .as_ref()
        .map(|diff| diff.files.clone())
        .unwrap_or_default();
    let not_repo = core
        .view
        .detail
        .diff
        .as_ref()
        .is_some_and(|diff| diff.not_repo);
    let loaded = core.view.detail.diff.is_some();
    let has_selection =
        !this.diff_selected_files.is_empty() || !this.diff_selected_hunks.is_empty();
    let all_collapsed = !files.is_empty()
        && files
            .iter()
            .all(|file| this.diff_collapsed_files.contains(&file.path));

    // 工具栏（docs/PRD.md「改动审查视图」）：折叠/展开文件树按钮左对齐，折叠/展开 diff
    // 区域按钮右对齐
    let toolbar = h_flex()
        .items_center()
        .gap_2()
        .px_3()
        .pb_2()
        .child(
            Button::new("diff-toggle-tree")
                .small()
                .ghost()
                .icon(if this.diff_tree_visible {
                    IconName::PanelLeftClose
                } else {
                    IconName::PanelLeftOpen
                })
                .tooltip(if this.diff_tree_visible {
                    "折叠文件树"
                } else {
                    "展开文件树"
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_diff_tree(cx))),
        )
        .child(div().flex_1())
        .child(
            Button::new("diff-toggle-changes")
                .small()
                .ghost()
                .icon(if all_collapsed {
                    IconName::PanelRightOpen
                } else {
                    IconName::PanelRightClose
                })
                .tooltip(if all_collapsed {
                    "展开 diff"
                } else {
                    "折叠 diff"
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_all_diffs(cx))),
        );

    let body = if not_repo {
        ui::empty_hint("当前工作目录不是 git 仓库", &theme).into_any_element()
    } else if !loaded {
        ui::empty_hint("正在加载改动…", &theme).into_any_element()
    } else if files.is_empty() {
        ui::empty_hint("暂无改动", &theme).into_any_element()
    } else {
        let mut content = h_flex().flex_1().min_h_0().min_w_0().gap_2();
        if this.diff_tree_visible {
            content = content.child(diff_tree(&files, this, cx));
        }
        content
            .child(diff_inline(&files, this, cx))
            .into_any_element()
    };
    let mut view = v_flex()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .gap_2()
        .px_3()
        .pb_3()
        .child(toolbar)
        .child(body);
    if has_selection && !not_repo && !files.is_empty() {
        view = view.child(diff_footer(this, cx));
    }
    view.into_any_element()
}

/// 左侧文件树：仅包含改动文件，点击文件滚动到对应改动。
fn diff_tree(files: &[GitDiffFile], this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let nodes = difftree::build(files);
    let mut rows = Vec::new();
    for node in &nodes {
        push_tree_rows(node, 0, this, cx, &mut rows);
    }
    v_flex()
        .id("diff-tree")
        .w(px(DIFF_TREE_WIDTH))
        .h_full()
        .min_h_0()
        .gap_1()
        .p_1()
        .bg(theme.muted)
        .rounded_md()
        .overflow_y_scroll()
        .children(rows)
        .into_any_element()
}

fn push_tree_rows(
    node: &DiffNode,
    depth: usize,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
    rows: &mut Vec<AnyElement>,
) {
    let theme = ui::Colors::of(cx.theme());
    let indent = rems(0.5 + depth as f32 * (TREE_INDENT / 16.0));
    if let Some(ix) = node.file_ix {
        rows.push(
            Button::new(SharedString::from(format!("diff-tree-{}", node.key)))
                .xsmall()
                .ghost()
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| this.scroll_to_file(ix, cx)))
                .child(
                    h_flex().w_full().justify_start().gap_1().pl(indent).child(
                        div().flex_1().min_w_0().child(
                            Label::new(node.name.clone())
                                .text_xs()
                                .truncate()
                                .text_color(theme.foreground),
                        ),
                    ),
                )
                .into_any_element(),
        );
        return;
    }

    let collapsed = this.diff_collapsed_dirs.contains(&node.key);
    rows.push(
        Button::new(SharedString::from(format!("diff-dir-{}", node.key)))
            .xsmall()
            .ghost()
            .w_full()
            .on_click(cx.listener({
                let key = node.key.clone();
                move |this, _, _, cx| this.toggle_diff_dir(key.clone(), cx)
            }))
            .child(
                h_flex()
                    .w_full()
                    .justify_start()
                    .gap_1()
                    .pl(indent)
                    .child(
                        Label::new(if collapsed { "▸" } else { "▾" })
                            .text_xs()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        Label::new(node.name.clone())
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .into_any_element(),
    );
    if collapsed {
        return;
    }
    for child in &node.children {
        push_tree_rows(child, depth + 1, this, cx, rows);
    }
}

/// 右侧 inline 改动：文件头 + hunk（行内容）+ 选择。
fn diff_inline(files: &[GitDiffFile], this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut pane = v_flex()
        .id("diff-inline")
        .flex_1()
        .min_w_0()
        .min_h_0()
        .pr_4()
        .overflow_y_scroll()
        .track_scroll(&this.diff_scroll);

    for file in files {
        let collapsed = this.diff_collapsed_files.contains(&file.path);
        pane = pane.child(diff_file_block(file, collapsed, this, cx));
    }
    div()
        .id("diff-scroll-wrap")
        .relative()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(pane)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(Scrollbar::vertical(&this.diff_scroll).id("diff-scrollbar")),
        )
        .into_any_element()
}

/// 单文件区块：文件头（选择/折叠）+ 各 hunk。
fn diff_file_block(
    file: &GitDiffFile,
    collapsed: bool,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let selected = this.diff_selected_files.contains(&file.path);
    let status = match file.status {
        amux_common::domain::GitChangeStatus::Added => ("A", theme.success),
        amux_common::domain::GitChangeStatus::Deleted => ("D", theme.danger),
        amux_common::domain::GitChangeStatus::Modified => ("M", theme.warning),
    };
    let mut block = v_flex().w_full().child(
        h_flex()
            .w_full()
            .h(rems(2.5))
            .px_2()
            .gap_2()
            .items_center()
            .bg(theme.muted.opacity(0.35))
            .border_t_1()
            .border_color(theme.border)
            .child(
                Checkbox::new(SharedString::from(format!("diff-sel-file-{}", file.path)))
                    .checked(selected)
                    .on_click(cx.listener({
                        let path = file.path.clone();
                        move |this, checked: &bool, _, cx| {
                            if *checked != this.diff_selected_files.contains(&path) {
                                this.toggle_diff_file_selected(path.clone(), cx);
                            }
                        }
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(
                        Label::new(file.path.clone())
                            .text_sm()
                            .font_family(theme.mono_font_family.clone())
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .flex_shrink_0(),
                    ),
            )
            .child(
                Label::new(format!("+{}", file.additions))
                    .text_xs()
                    .text_color(theme.success),
            )
            .child(
                Label::new(format!("-{}", file.deletions))
                    .text_xs()
                    .text_color(theme.danger),
            )
            .child(
                gpui_component::tag::Tag::custom(
                    status.1.opacity(0.14),
                    status.1,
                    status.1.opacity(0.35),
                )
                .small()
                .rounded_full()
                .child(Label::new(status.0).text_xs()),
            ),
    );

    if !collapsed {
        for hunk in &file.hunks {
            let key = (file.path.clone(), hunk.header.clone());
            let hunk_selected = this.diff_selected_hunks.contains(&key);
            block = block.child(
                h_flex()
                    .w_full()
                    .h(rems(1.75))
                    .items_center()
                    .gap_2()
                    .px_2()
                    .bg(theme.primary.opacity(0.12))
                    .child(
                        Checkbox::new(SharedString::from(format!(
                            "diff-sel-hunk-{}-{}",
                            file.path, hunk.header
                        )))
                        .checked(hunk_selected)
                        .on_click(cx.listener({
                            let path = file.path.clone();
                            let header = hunk.header.clone();
                            move |this, checked: &bool, _, cx| {
                                let key = (path.clone(), header.clone());
                                if *checked != this.diff_selected_hunks.contains(&key) {
                                    this.toggle_diff_hunk_selected(
                                        path.clone(),
                                        header.clone(),
                                        cx,
                                    );
                                }
                            }
                        })),
                    )
                    .child(
                        Label::new(hunk.header.clone())
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.primary),
                    ),
            );
            let numbers = diff::line_numbers(&hunk.header, &hunk.lines);
            for (index, line) in hunk.lines.iter().enumerate() {
                let (background, marker, marker_color) = match line.kind {
                    GitDiffLineKind::Add => (theme.success.opacity(0.16), "+", theme.success),
                    GitDiffLineKind::Remove => (theme.danger.opacity(0.16), "-", theme.danger),
                    GitDiffLineKind::Context => (theme.popover, " ", theme.muted_foreground),
                };
                let numbers = numbers[index];
                block = block.child(
                    h_flex()
                        .w_full()
                        .h(rems(1.375))
                        .items_center()
                        .bg(background)
                        .child(number_gutter(numbers.old, &theme))
                        .child(number_gutter(numbers.new, &theme))
                        .child(
                            div().w(rems(1.5)).h_full().flex().justify_center().child(
                                Label::new(marker)
                                    .text_xs()
                                    .font_family(theme.mono_font_family.clone())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(marker_color),
                            ),
                        )
                        .child(
                            Label::new(line.text.clone())
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .whitespace_nowrap()
                                .flex_shrink_0(),
                        ),
                );
            }
        }
    }
    block.into_any_element()
}

/// diff 行号栏（该侧无行号时留空）。
fn number_gutter(number: Option<usize>, theme: &ui::Colors) -> AnyElement {
    div()
        .w(rems(3.))
        .h_full()
        .px_2()
        .flex()
        .justify_end()
        .border_r_1()
        .border_color(theme.border.opacity(0.45))
        .child(
            Label::new(number.map(|n| n.to_string()).unwrap_or_default())
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground),
        )
        .into_any_element()
}

/// 审查视图底部：选中统计与引用到会话输入框。
///
/// 引用而非直接发送：文件引用复制文件路径、代码块引用复制代码块内容到会话输入框
/// （docs/PRD.md「改动审查」）。
fn diff_footer(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let selected_files = this.diff_selected_files.len();
    let selected_hunks = this.diff_selected_hunks.len();
    h_flex()
        .gap_2()
        .items_center()
        .child(
            Label::new(format!(
                "已选 {selected_files} 文件 · {selected_hunks} 代码块"
            ))
            .text_xs()
            .text_color(theme.muted_foreground),
        )
        .child(
            Button::new("diff-clear-selection")
                .small()
                .label("清空选择")
                .on_click(cx.listener(|this, _, _, cx| this.clear_diff_selection(cx))),
        )
        .child(div().flex_1())
        .child(
            Button::new("diff-reference-selected")
                .small()
                .primary()
                .label("引用到输入框")
                .tooltip("引用到会话输入框，补充指令后发送")
                .on_click(
                    cx.listener(|this, _, window, cx| this.reference_diff_selection(window, cx)),
                ),
        )
        .into_any_element()
}
