# amux — 系统设计

**详略约定**：本文档只描述框架，不描述细节；实现细节交由实现 agent 决定。

---

## 1. 架构

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

- **Client（仅 GUI 客户端）**：amux 桌面应用（Tauri），是唯一的客户端，直连各已注册机器的 server——本机与远程同等对待，统一注册后连接。每条连接对应一台机器，使用同一套协议。
- **Server**：每台机器运行一个常驻进程（TypeScript），持有本机 AHAL Driver 实例，负责会话生命周期、事件持久化、git 能力。**server 之间不通信**——每个 server 只服务本机会话，对连接方一律按 GUI 客户端对待。

职责划分：

- **会话数据主权**：每个会话的会话历史、元数据只存于该会话所在机器的 server；GUI 不复制远程会话历史
- **GUI = 聚合层**：会话列表由 GUI 汇总各 server 的连接编排；跨机器工作流是 GUI 内部编排，基于会话原语实现，不占用协议面；远程 server 离线时其会话标为不可达
- **多设备共存**：任意数量的 GUI 可同时连接同一 server、查看并操作同一会话，互不踢出
- **生命周期解耦**：任何客户端断开（含桌面应用关闭）不停止 server、不销毁会话；会话仅由显式关闭 / 删除结束

## 2. Server 生命周期与启动

每台机器（含本机）统一运行一个 server 常驻进程，与任何客户端连接无关：

- **启动**：server 由所在机器自行启动（手动命令、系统服务或安装脚本），GUI 不负责拉起——连接失败即视为该机器离线；认证 token 每次启动需指定（见 §3）
- **关闭**：GUI 关闭只断开 socket；server 继续常驻，agent 任务与事件落盘不受影响
- **机器重启**：重启机器后需重新启动 server，会话从磁盘注册表恢复（崩溃前忙状态的会话标为 interrupted，由客户端决定是否 resume）
- **GUI 视角**：本机与远程完全一致——注册、连接、认证、离线处理无差别

## 3. 传输与消息

- 传输统一为 **WebSocket**，消息格式为 **JSON-RPC 2.0**：请求必须回响应，事件以通知（无响应）表达
- **认证**：所有连接统一携带 token；**token 不落盘，每次启动由用户指定**（`--token` 或环境变量 `AMUX_TOKEN`，统一名称 token，无别名），未指定则 server 拒绝启动
- **会话 ID**：server 直接颁发全局唯一的会话 ID；机器归属由连接推断（每条连接对应一台机器），请求发给会话所属的 server 连接
- 断线后客户端指数退避重连
- 单用户信任模型：无用户体系与权限系统（PRD 边界），安全性依赖运行环境

### 3.1 事件交付

- **广播**：server 向所有已连接客户端持续推送每条通知（AHAL 事件 + 用户消息 `user_message`），多客户端收到同一份流、互不踢出；无订阅机制
- **顺序**：推送顺序 = 会话内记录顺序（jsonl 追加顺序 / 实时到达顺序）；跨机器时间线用时间戳（各机时钟偏差为已知限制）
- **连接补齐（server 按连接对齐）**：客户端连接时先获取会话历史（`get_history`，持久化的对话内容，按 jsonl 顺序）；server 记录该连接在各会话上的补齐位置，连接建立后到达的实时项由 server 按连接暂存，按序补齐历史之后的缺口后并入实时广播——客户端按序追加即可，天然无重复、无需去重；补齐期间未收到的在飞流式片段（`*_chunk`）由随后到达的完整消息（`agent_message` 等）收敛为完整内容

## 4. 会话

- 交互只有两个动作：**prompt**（唯一消息入口：idle 启动新工作、忙时 steer；输入内容为文本 / 内嵌资源 / 资源引用）与 **cancel**（取消进行中的工作）
- **用户输入持久化**：server 将用户 prompt 记入会话历史（与事件同一 jsonl、按追加顺序），并以 `user_message` 通知广播——事件流只含 agent 侧输出，用户输入由历史记录补全对话
- **按钮映射**（客户端本地配置，无专用协议）：commit / submit PR 等需要编写内容的操作经 prompt 由 agent 执行；push、undo / revert（文件 / hunk / 全部）等无需判断的操作由 server 直连 git 执行——undo/revert 需等工作区间结束后再触发，否则"撤销最近变更"的时点语义是乱的；skill 安装 / 更新经 prompt 由 agent 执行（见「Skills 管理」）；新会话 / Kill Session 由客户端直接发起对应会话操作
- **多客户端并发**：server 对同一会话的所有 prompt（含各客户端的）按到达顺序串行化，保证按调用顺序送达
- **通知**：客户端从事件流自行推导（工作结束 / 异常 / 长时间无响应），配置存客户端本地，无需协议

## 5. 数据存储

- server 与 GUI 的本地数据统一存放于 `~/.amux`（各自子目录），GUI 侧的机器注册表、技能注册表等配置也在其中
- worktree 统一创建于 `~/.amux/worktrees`
- **server 会话历史以 JSONL 落盘**（`~/.amux/server/history/<sessionId>.jsonl`，每个会话一个文件）：
  - 一行一条记录，**对话内容按追加顺序**（jsonl 只追加不修改，物理行序即真实对话顺序）
  - **只保存对话内容**：用户输入、agent 思考（`agent_thought`）、工具调用（`tool_call_update`）、agent 输出（`agent_message`）——状态变化、用量、错误、流式片段（`*_chunk`）**不落盘**，仅实时流转
- **会话注册表**（`~/.amux/server/sessions.json`）：会话元数据（harness / cwd / 模型 / 最后状态 / closed / interrupted 等），供重启恢复与 interrupted 标记
- **认证 token 不落盘**：每次启动由 `--token` 或环境变量 `AMUX_TOKEN` 指定（统一名称 token，无别名）；未指定则拒绝启动（token 随进程内存存在，重启需重新指定）
- **重连补齐（server 按连接对齐）**：不设按会话的有界流式缓冲——客户端重连先取历史（`get_history`），补齐期间到达的实时项由 server 按连接暂存、按序补齐后并入广播（补齐完成即清空，无持久化）

## 6. Skills 管理

Skills 注册表（URL + 本地目录 + 作用域 + 启用状态）是用户配置，存于 **GUI 客户端**（GUI 本地配置）——增删 / 启停是 GUI 本地操作，多设备各自配置。

Skill 的安装 / 更新**像按钮一样由用户触发**：GUI 按注册表拼接一段 prompt（如"克隆 `{url}` 到 `{localDir}` 并启用"），发给某个会话的 agent 执行 clone/pull——与 commit 按钮同属"按钮 = 拼接 prompt"的模式。作用域决定该 skill 的按钮出现在哪些会话（global 全部、project 特定 repo 的会话、personal 自用）。

## 7. Server 与 Agent Harness 通信

Server 与 Agent Harness 之间通过 [AHAL](AHAL.md) 层通信。AHAL 提供统一的 Driver/Session 接口，屏蔽不同 harness 的差异。

## 8. 工作流

工作流引擎运行在 GUI 客户端（server 之间不通信），基于会话原语编排（见「会话」）：模板与实例是 GUI 本地数据，任务通过创建会话、发送 prompt、取消工作等原语组合表达，不占用协议面。

已知取舍：关闭应用后各机器上已启动的 agent 任务继续运行，但工作流推进逻辑（排序、审查门、失败策略）随 GUI 退出而停止；若要求跨关闭存活，需把引擎下沉到 server 并引入 server 间通信。

## 9. 参考

- [raft.build](https://raft.build)：Client-Server + WebSocket 的桌面应用架构参考
- [herdr](https://github.com/ogulcancelik/herdr)：终端 agent 多路复用，server 常驻与 attach/reattach 模式
- [t3code](https://github.com/pingdotgg/t3code)：agent 控制面——provider 驱动注册、按 turn 的 git checkpoint、事件溯源思路
