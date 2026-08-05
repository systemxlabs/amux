# amux — 系统设计

版本: 0.3
日期: 2026-08-05

---

## Client-Server 架构

采用 Client-Server 架构，Client 与 Server 之间通过 WebSocket 通信，参考 [raft.build](https://raft.build) 的设计模式。通信协议见 [PROTOCOL.md](PROTOCOL.md)。

- Server（daemon）是每台机器上的常驻服务，是 AHAL 的唯一客户端
- Client 只有 GUI 客户端：直连各已注册机器的 daemon（本机与远程同等对待，统一注册后连接）；daemon 之间不通信
- 会话注册表、事件历史、技能配置存于会话所在机器的 daemon；机器注册表与聚合视图在 GUI 客户端

## Daemon 生命周期与启动

每台机器（含本机）统一运行一个 daemon 常驻进程，与任何客户端连接无关：

- **启动**：daemon 由所在机器自行启动（手动命令、系统服务或安装脚本），GUI 不负责拉起——连接失败即视为该机器离线
- **关闭**：GUI 关闭只断开 socket；daemon 继续常驻，agent 任务与事件落盘不受影响
- **机器重启**：重启机器后需重新启动 daemon，会话从磁盘注册表恢复（崩溃前忙状态的会话标为 interrupted，由客户端决定是否 resume）
- **GUI 视角**：本机与远程完全一致——注册（本机 URL 为 `ws://127.0.0.1:19770`）、连接、认证、离线处理无差别

## 数据存储

几乎所有数据都存储在 Server 端，Client 仅作展示。

- 每个会话的完整事件流由所在 daemon 持久化（每条事件分配单调递增 `seq`），daemon 向所有已连接客户端广播实时事件；客户端重连后以 `session.history`（`afterSeq`）补齐缺口
- 机器重启后 daemon 从磁盘注册表重建会话列表，崩溃前忙状态的会话标为 interrupted，由客户端决定是否 resume

## Skills 管理

Skills 管理仅存储 skills 仓库位置（URL + 本地目录），不存储 skills 具体内容。Skills 的安装和更新由 agent 执行。注册表存于 GUI 客户端（同机器注册表），会话创建时经 `session.create.skills` 传给 daemon 拼入初始 prompt（PROTOCOL.md §10）。

## Server 与 Agent Harness 通信

Server 与 Agent Harness 之间通过 AHAL 层通信。AHAL 提供统一的 Driver/Session 接口，屏蔽不同 harness 的差异。

## 工作流

工作流引擎运行在 GUI 客户端（daemon 之间不通信），基于协议层的会话原语编排（PROTOCOL.md §5-§6）：模板与实例是 GUI 本地数据，任务通过 `session.create` / `session.prompt` / `session.cancel` / `session.history` 组合表达，不占用协议面。

已知取舍：关闭应用后各机器上已启动的 agent 任务继续运行，但工作流推进逻辑（排序、审查门、失败策略）随 GUI 退出而停止；若要求跨关闭存活，需把引擎下沉到 daemon 并引入 daemon 间通信。
