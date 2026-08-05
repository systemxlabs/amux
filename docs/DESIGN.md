# amux — 系统设计

**详略约定**：本文档只描述框架，不描述细节；实现细节交由实现 agent 决定。

---

## 架构

采用 Client-Server 架构：GUI 桌面应用（Client）与各机器上的 server 常驻进程之间通过 WebSocket 通信，参考 [raft.build](https://raft.build) 的设计模式。

```
┌──────────────┐   WS(注册)   ┌──────────────┐
│              │────────────►│ 机器 A server │──► AHAL → harness
│  GUI 桌面     │             └──────────────┘
│  (聚合视图)   │   WS(注册)   ┌──────────────┐
│              │────────────►│ 机器 B server │──► AHAL → harness
└──────────────┘             └──────────────┘
                     （机器 A/B 无本机/远程之分，含本机）
```

- **Client（仅 GUI 客户端）**：amux 桌面应用，是唯一的客户端，直连各已注册机器的 server——本机与远程同等对待，统一注册后连接。每条连接对应一台机器，使用同一套协议。
- **Server**：每台机器运行一个常驻进程，持有本机 AHAL Driver 实例，负责会话生命周期、事件持久化、git 能力。**server 之间不通信**——每个 server 只服务本机会话，对连接方一律按 GUI 客户端对待。

职责划分：

- **会话数据主权**：每个会话的历史事件流、元数据只存于该会话所在机器的 server；GUI 不复制远程会话历史
- **GUI = 聚合层**：会话列表由 GUI 汇总各 server 的连接编排；跨机器工作流是 GUI 内部编排，基于会话原语实现，不占用协议面；远程 server 离线时其会话标为不可达
- **多设备共存**：任意数量的 GUI 可同时连接同一 server、查看并操作同一会话，互不踢出
- **生命周期解耦**：任何客户端断开（含桌面应用关闭）不停止 server、不销毁会话；会话仅由显式关闭 / 删除结束

## Server 生命周期与启动

每台机器（含本机）统一运行一个 server 常驻进程，与任何客户端连接无关：

- **启动**：server 由所在机器自行启动（手动命令、系统服务或安装脚本），GUI 不负责拉起——连接失败即视为该机器离线
- **关闭**：GUI 关闭只断开 socket；server 继续常驻，agent 任务与事件落盘不受影响
- **机器重启**：重启机器后需重新启动 server，会话从磁盘注册表恢复（崩溃前忙状态的会话标为 interrupted，由客户端决定是否 resume）
- **GUI 视角**：本机与远程完全一致——注册、连接、认证、离线处理无差别

## 传输与消息

- 传输统一为 **WebSocket**，消息格式为 **JSON-RPC 2.0**：请求必须回响应，事件以通知（无响应）表达
- **认证**：所有连接统一携带 token（安装 server 时生成，仅展示一次）
- **会话 ID**：server 直接颁发全局唯一的会话 ID；机器归属由连接推断（每条连接对应一台机器），请求发给会话所属的 server 连接
- 断线后客户端指数退避重连，重连后以会话历史恢复状态（见「事件流」）
- v1 不做协议版本协商——server 较旧时，其不认识的方法以 method not found 自然暴露
- 单用户信任模型：无用户体系与权限系统（PRD 边界），安全性依赖运行环境

## 会话

- 交互只有两个动作：**prompt**（唯一消息入口：idle 启动新工作、忙时 steer；输入内容为文本 / 内嵌资源 / 资源引用）与 **cancel**（取消进行中的工作）
- **按钮映射**（客户端本地配置，无专用协议）：commit / submit PR 等需要编写内容的操作经 prompt 由 agent 执行；push、undo / revert（文件 / hunk / 全部）等无需判断的操作由 server 直连 git 执行——undo/revert 需等工作区间结束后再触发，否则"撤销最近变更"的时点语义是乱的；skill 安装 / 更新经 prompt 由 agent 执行（见「Skills 管理」）；新会话 / Kill Session 由客户端直接发起对应会话操作
- **多客户端并发**：server 对同一会话的所有 prompt（含各客户端的）按到达顺序串行化，保证按调用顺序送达
- **通知（P1）**：客户端从事件流自行推导（工作结束 / 异常 / 长时间无响应），配置存客户端本地，无需协议

## 事件流

- **广播**：server 向所有已连接客户端持续推送每条事件通知（AHAL 事件透传），**连接即收流，无订阅机制**；多客户端收到同一份事件流，互不踢出
- **顺序**：推送顺序 = AHAL 投递顺序；跨机器时间线用时间戳（各机时钟偏差为已知限制）
- **容错**：事件流是瞬时的、不持久化——客户端断开期间错过的事件不重放，重连后以会话历史恢复状态（见「数据存储」）；`idle` 后不再出现上一区间的消息/工具事件

## 数据存储

- server 与 GUI 的本地数据统一存放于 `~/.amux`（各自子目录），GUI 侧的机器注册表、技能注册表等配置也在其中
- worktree 统一创建于 `~/.amux/worktrees`
- server 持久化**会话历史**（对话内容），不存 agent 流式事件——事件流仅实时广播、不持久化

## 读写策略

- **需要判断的写操作走 agent**：如 commit——需要 agent 编写 commit message，经 prompt 由 agent 执行
- **其余写操作由 server 直连 git**：push、undo / revert 等不需要 agent 判断的操作，由 server 直接执行 git 命令
- **读操作由 server 直连 git**：diff 视图、worktree 查看等实时只读信息由 server 直连 git 提供（GUI 无法靠 prompt 实时渲染）；需要 server 具备 git 能力

## Skills 管理

Skills 注册表（URL + 本地目录 + 作用域 + 启用状态）是用户配置，存于 **GUI 客户端**（GUI 本地配置）——增删 / 启停是 GUI 本地操作，多设备各自配置。

Skill 的安装 / 更新**像按钮一样由用户触发**：GUI 按注册表拼接一段 prompt（如"克隆 `{url}` 到 `{localDir}` 并启用"），发给某个会话的 agent 执行 clone/pull——与 commit 按钮同属"按钮 = 拼接 prompt"的模式。作用域决定该 skill 的按钮出现在哪些会话（global 全部、project 特定 repo 的会话、personal 自用）。

## Server 与 Agent Harness 通信

Server 与 Agent Harness 之间通过 [AHAL](AHAL.md) 层通信。AHAL 提供统一的 Driver/Session 接口，屏蔽不同 harness 的差异。

## 工作流

工作流引擎运行在 GUI 客户端（server 之间不通信），基于会话原语编排（见「会话」）：模板与实例是 GUI 本地数据，任务通过创建会话、发送 prompt、取消工作等原语组合表达，不占用协议面。

已知取舍：关闭应用后各机器上已启动的 agent 任务继续运行，但工作流推进逻辑（排序、审查门、失败策略）随 GUI 退出而停止；若要求跨关闭存活，需把引擎下沉到 server 并引入 server 间通信。

## 参考

- [raft.build](https://raft.build)：Client-Server + WebSocket 的桌面应用架构参考
- [herdr](https://github.com/ogulcancelik/herdr)：终端 agent 多路复用，server 常驻与 attach/reattach 模式
- [t3code](https://github.com/pingdotgg/t3code)：agent 控制面——provider 驱动注册、按 turn 的 git checkpoint、事件溯源思路
