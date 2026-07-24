# amux — Product Requirements Document

版本: 0.1
日期: 2026-07-21

amux 是一个 tmux-like 桌面应用 for Agentic Coding。

---

## 会话管理

用户可以在多台机器上创建、管理 agent 会话（codex、claude code、kimi code 等），指定工作目录即可启动。会话中用户实时看到 agent 的思考过程、文本输出、工具调用。用户可以在 agent 工作过程中发送新指令干预其行为，也可以随时取消当前工作。关闭会话后对话历史保留，之后可以恢复继续。

## 会话历史

会话的完整事件流持久化存储。用户可随时滚动回溯任意历史位置，回溯期间新事件正常收但不强制滚动到底（用户可一键跳回底部）。支持在历史中搜索文本，支持从历史中选中内容复制。Attach 后自动拉取持久化的历史，即使 detach 期间产生的输出也不会丢失。

## 会话组织

用户可为会话添加标签和备注，便于后续查找。支持按机器、harness 类型、标签、状态（活跃/空闲/已关闭）、关键字（会话名称、备注、工作目录、prompt 内容）搜索筛选。支持置顶常用会话、归档旧会话。

## Attach / Detach

支持 tmux 式的 attach/detach 模型：detach 后会话在后台继续运行不受影响；之后可以从任意机器 attach 回来，立即看到当前状态和最近的输出，继续接收实时事件。多个用户可以同时 attach 到同一个会话，都能看到完整输出和当前状态，任一用户可以发送 prompt。应能看到当前有哪些用户 attach 在此会话。

## 多机器管理

用户可以看到所有已注册机器的列表及在线状态。每台机器自动发现本机已安装的 agent harness，用户也可手动配置路径和默认模型。用户可以浏览所有机器上的所有活跃会话，按机器或 harness 类型筛选。

## 通知

会话状态变更时发送桌面通知：工作结束（含结束原因）、异常终止、长时间无响应。用户可按会话或全局配置通知开关和触发条件。多机器场景下，通知携带机器和会话信息，用户点击通知可直接 attach 到对应会话。

## 快捷按钮

会话界面提供一键快捷按钮：Submit PR（暂存、commit、推送、创建 PR）、Commit、Push、Undo（撤销 agent 最近文件变更）、New Session、Kill Session。快捷按钮应可自定义——用户可以增删按钮、修改按钮对应的命令。

## Worktree 管理

用户可以查看任意机器上的所有 git worktree，包括路径、分支、clean/dirty 状态、关联的会话。支持从分支或 commit 创建新 worktree（可同时创建关联的 agent 会话）、删除 worktree（有活跃会话时警告）、锁定 worktree 防止误删。

## Diff Review

用户可以在会话中实时查看当前工作区的 git diff——文件列表和行数统计，点击文件查看 side-by-side 或 inline diff 并带有语法高亮。支持对单个文件、单个 hunk 或全部变更执行 revert。可以在 diff 视图中直接对特定代码片段发送 prompt。支持对比两个不同会话产生的变更。

## Skills 管理

用户添加 skills 时只需指定仓库 URL 和本地目录，系统只存储这两项信息。skills 的安装和更新：会话启动时，系统将已启用的 skills 拼成 prompt（例如"请安装/更新以下 skills：repo1 URL → dir1, repo2 URL → dir2"）发给 agent，由 agent 执行实际的 clone/pull。skills 可设为全局、项目级（关联特定 git repo 的会话自动加载）、或个人级。每个 skills 仓库可单独启用或禁用。

## 工作流

工作流由一组任务组成，每个任务指定在哪台机器上执行、使用哪种 agent harness、发送什么 prompt、等待哪些前置任务完成后才开始。任务间可以有依赖关系，支持串行和并行。工作流手动触发后按依赖自动调度到对应机器的 agent 执行，全程可视化展示进度。执行期间用户可以取消整个工作流、跳过某个任务、或重试失败任务。系统预置常用模板（实现+Review、多模型对比、CI 修复、代码迁移），用户可从模板修改另存，也支持导出/导入工作流定义。

## 参考
1. herdr https://github.com/ogulcancelik/herdr
2. freebuddy https://github.com/maojindao55/freebuddy