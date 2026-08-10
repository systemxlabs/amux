# amux — 系统设计

**详略约定**：本文档只描述框架，不描述细节；实现细节交由实现 agent 决定。

---

## 1. 架构

Client-Server 架构：GUI 桌面应用（**GPUI**）与各机器上的 server 常驻进程通过 WebSocket 通信；server 作为 **ACP v1 client** 对接各 agent（Codex / Claude / Kimi），经 ACP 的 stdio 传输 spawn agent 子进程。

```
┌──────────────┐   WS(JSON-RPC)  ┌───────────────┐  ACP v1 (stdio) ┌────────────┐
│  GUI 桌面     │◄───────────────►│ 机器 server   │◄───────────────►│ agent      │
│ (GPUI)       │                 │ (常驻)         │   spawn 子进程   │ codex-acp  │
│  ▲           │                 │  ACP client    │                │ claude-acp │
│  │ 会话历史    │                 └───────────────┘                │ kimi acp   │
│  └ 本地缓存   │                                                    └────────────┘
└──────────────┘
```

- **Client（仅 GUI 客户端）**：amux 桌面应用，基于 **GPUI + gpui-component**，直连各已注册机器的 server——本机与远程同等对待，统一注册后连接。每条连接对应一台机器，使用同一套协议。
- **Server**：每台机器运行一个常驻进程，是 **ACP v1 client**——spawn agent 子进程、驱动 ACP 会话、把 agent 输出聚合后交付给 GUI、直连 git。**server 之间不通信**——每个 server 只服务本机会话，对连接方一律按 GUI 客户端对待
- **协议单一来源**：app↔server 协议（方法面、参数/结果类型、通知类型）由共享 crate 定义，GUI 与 server 从同一处导入——单一语言实现，无需双语言协议对齐

职责划分：

- **历史权威 = agent**：会话历史的唯一真相源在 agent 侧；server 不保存
- **GUI = 聚合层**：会话列表由 GUI 汇总各 server（server 从 agent `session/list` 获得）；跨机器工作流是 GUI 内部编排，基于会话原语实现，不占用协议面；远程 server 离线时其会话标为不可达
- **多设备共存**：任意数量的 GUI 可同时连接同一 server、查看并操作同一会话，互不踢出；各 GUI 独立加载与缓存会话数据
- **生命周期解耦**：任何客户端断开（含桌面应用关闭）不停止 server、不销毁会话；会话仅由显式关闭 / 删除结束

## 2. 代码仓库结构（workspace crate）

| crate | 职责 |
|---|---|
| `protocol` | app↔server 协议面：方法名、参数/结果类型、通知类型。**协议的唯一来源**，GUI 与 server 均从这里导入 |
| `server` | 每台机器的常驻进程：WebSocket 传输（tokio-tungstenite）、JSON-RPC 分发、会话管理、会话数据聚合与 activities 缓存、git 能力（status/diff/push/revert）；经 ACP 官方 SDK（`agent-client-protocol`）与 agent 通信 |
| `gui` | GPUI 桌面应用：三面板视图（Dock 布局）、对话流、会话活动、diff 编辑器、侧边栏、设置页；会话历史本地缓存；内置编排 agent（rig） |

- server 与 agent 的交互**只经 ACP 协议**（官方 SDK `agent-client-protocol` + `agent-client-protocol-tokio`）
- server 之间不通信；跨机器编排在 GUI 侧完成
- 技术依赖：`tokio`（异步）、`serde`/`serde_json`（JSON-RPC）、`tokio-tungstenite`（WS）、`agent-client-protocol`（ACP 官方 SDK）、`rig`（编排 agent 的 LLM 客户端与工具抽象）、`gpui` + `gpui-component`（GUI，跟踪 Zed 主线 git 依赖）

## 3. Server 生命周期与启动

每台机器（含本机）统一运行一个 server 常驻进程（单二进制），与任何客户端连接无关：

- **启动**：server 由所在机器自行启动（手动命令、系统服务或安装脚本），GUI 不负责拉起——连接失败即视为该机器离线；认证 token 每次启动需指定
- **关闭**：GUI 关闭只断开 socket；server 继续常驻，agent 子进程与 ACP 会话不受影响
- **机器重启**：server 无持久化状态（历史在 agent 侧、配置可重建）——重启后重新 spawn agent 子进程，会话列表经 ACP `session/list` 从 agent 侧恢复
- **GUI 视角**：本机与远程完全一致——注册、连接、认证、离线处理无差别

## 4. 传输与消息（GUI ↔ server）

- 传输统一为 **WebSocket**（tokio-tungstenite），消息格式为 **JSON-RPC 2.0**：请求必须回响应，事件以通知（无响应）表达
- **认证**：所有连接统一携带 token；**token 不落盘，每次启动由用户指定**（`--token` 或环境变量 `AMUX_TOKEN`，统一名称 token，无别名），未指定则 server 拒绝启动
- **会话 ID**：server 直接颁发全局唯一的会话 ID；机器归属由连接推断（每条连接对应一台机器），请求发给会话所属的 server 连接
- 断线后客户端指数退避重连
- 单用户信任模型：无用户体系与权限系统，安全性依赖运行环境

## 5. 会话数据（事件交付 · 历史 · 活动）

会话数据指一个会话的全部内容：对话内容（用户消息 + agent 输出）与会话活动（activities）。本节统一描述它们的交付、存储与获取。

### 5.1 交付模型

- **非流式交付**：agent 工作以 turn 为单位。server 接收 agent 的流式事件（ACP `session/update`），在 turn 结束时**聚合交付**——把整个 agent 输出作为一条完整消息推送给 GUI。GUI 不接收逐 chunk 的流式传输
- **会话状态**：进行中 / 完成（turn 边界）由 server 推送

### 5.2 会话历史

- **权威 = agent**：会话历史的唯一真相源在 agent 侧（ACP session 持久化）；server 不保存
- GUI 打开会话时全量加载对话内容，缓存到本地（GUI 数据目录，按会话一个缓存文件）：
  - **打开会话**：对话内容全量加载（经 ACP `session/load` 重放聚合）→ 写入本地缓存
  - **增量**：turn 结束后的新输出追加到缓存
  - **resume（重新恢复会话）**：**清空旧缓存，重新全量加载**——不保留跨 resume 的缓存，保证缓存与 agent 侧历史一致
  - **会话删除**（ACP `session/delete`）：历史随 agent 侧删除而消失，不可恢复
  - 缓存仅作会话打开期间的读写（滚动回溯、分页），不承担历史权威

### 5.3 会话活动（activities）

- 中间活动（thinking / tool call / compaction 等）经 ACP 事件聚合产生；**同类连续事件合并为一条**（thinking 逐块累积、同一工具调用合并），turn 结束写入历史
- **实时活动**：turn 进行中，合并后的当前活动经 `activity` 通知**流式推送**（GUI 实时活动条与活动视图追加展示），空闲时清空
- **server 有界缓存**：按会话保留最近若干条活动（非持久化，超出淘汰最旧）
- GUI 在活动视图需要时经 `get_activities` **主动获取**（中间下方实时一条 + 右侧完整历史）

### 5.4 打开会话与重连

- **打开会话**：GUI 打开会话时，server 经 ACP `session/load` **全量重放**并聚合为对话内容交付给 GUI；`session/load` 的响应即**重放边界**
- **GUI 重连**：GUI 重连后重新打开会话（再次 `session/load`），重新聚合加载

### 5.5 本地数据

- server 其余本地数据存放于 `~/.amux/server`（配置等）；**认证 token 不落盘**
- GUI 本地数据存放于 `~/.amux/gui`：机器注册表、快捷指令、Skills 注册表、会话历史缓存、编排 agent 会话状态

## 6. 会话（交互）

- 交互只有两个动作：**prompt**（唯一消息入口：idle 启动新工作、忙时 steer；输入内容为文本 / 内嵌资源 / 资源引用）与 **cancel**（取消进行中的工作），经 ACP `session/prompt` / `session/cancel` 到达 agent
- **steer（忙时 prompt）**：忙时 prompt 的行为取决于 agent 实现（ACP v1 turn 模型），不支持进行中注入时 server 直接报错
- **用户输入**：GUI 的 prompt 经 server 转发给 agent；用户消息同时由 GUI 本地立即渲染（不依赖回显），并保留在对话内容中
- **快捷指令**（客户端本地配置，无专用协议）：每条指令是一段发给 agent 的提示词，经 prompt 由 agent 执行（Commit & Push、Submit PR、skill 安装 / 更新等，见「Skills 管理」）；直连 git 的 push / undo / revert 等操作不属于快捷指令；新会话 / Kill Session 由客户端直接发起对应会话操作
- **多客户端并发**：server 对同一会话的所有 prompt（含各客户端的）按到达顺序串行化，保证按调用顺序送达

## 7. GUI（GPUI 桌面应用）

GUI 为单进程桌面应用（GPUI + gpui-component，跟踪 Zed 主线 git 依赖；跨平台 macOS / Linux / Windows）：

- **布局**：三面板 **Dock 布局**（gpui-component）；右侧上下文面板（diff / 会话详情 / 会话活动）展开时**窗口向右扩展**，不压缩中间面板空间，关闭时收回
- **对话流**：只展示用户消息与 agent 输出的消息气泡（**Markdown 渲染**，gpui-component）+ 虚拟化列表；输出为完整消息，非流式
- **会话活动**：中间面板下方展示**正在进行的活动**（一条或无，实时）；右侧面板展示完整活动历史（上下滚动）——经 `get_activities` 获取
- **Diff Review**：代码编辑器组件 + **Tree Sitter 语法高亮**（gpui-component）；文件列表、side-by-side/inline diff、revert 操作
- **输入与设置**：输入区（多行、拖拽/粘贴、@ 引用）、快捷指令栏、设置页（机器管理）——gpui-component 表单/对话框组件
- **异步模型**：GPUI executor 承载 UI，WS 连接与 ACP 重放流经 **tokio** 运行，事件桥接进 GPUI 事件循环
- **本地配置**：机器注册表、快捷指令、Skills 注册表、编排 agent 会话状态（GUI 数据目录；会话历史缓存）

## 8. 日志与追踪（可观测性）

日志是 amux 调试的主要手段：GUI ↔ server ↔ ACP client ↔ agent 跨进程、跨机器，问题定位依赖能串起整条链路的日志。

## 9. Server 与 Agent 通信（ACP v1）

Server 作为 **ACP v1 client**（依赖官方 SDK `agent-client-protocol`）与各 agent 通信：

- **传输**：ACP stdio——server spawn agent 子进程（`codex-acp` / `claude-acp` / `kimi acp`），JSON-RPC 2.0 over stdin/stdout；每条消息单行 JSON，无内嵌换行
- **会话生命周期**：
  - `session/new`：新建会话（yolo 模式启动，见下）
  - `session/load`：加载会话并**全量重放历史**（`session/update` 通知流，重放完才响应）
  - `session/resume`：恢复会话上下文（不重放历史；历史加载走 `session/load`）
  - `session/prompt` / `session/cancel` / `session/delete` / `session/list`
- **事件聚合**：agent 的 `session/update` 通知 → server 聚合为完整输出（消息按 ID 收敛）与 activities（thinking / tool call / compaction 等）；输出交付与 activities 缓存见前文
- **会话状态**：由 ACP 会话信息（`session_info_update`）与 prompt/turn 生命周期推导（忙 / 就绪），推送为 GUI 的会话状态展示
- **权限（yolo）**：agent 经 `session/request_permission` 请求权限；server **自动批准**（yolo 模式，既定决策延续，无审批往返），安全性依赖运行环境
- **steer**：ACP v1 为 turn 模型，prompt 启动一个 turn、turn 结束（agent 回到就绪）后才可再 prompt。忙时 prompt 行为取决于 agent 实现（部分 agent 支持进行中注入）；若 agent 不支持进行中注入，server **直接向用户报错**（不排队、不静默降级）

## 10. 工作流

工作流由 **GUI 内置编排 agent** 驱动（**rig 单 turn 模式**实现），基于会话原语实现，不占用协议面：

- **编排 agent 会话**：工作流创建一个编排 agent 会话（GUI 内置 agent，状态存 GUI 本地），与普通会话一样支持 prompt / 状态（idle / thinking）
- **rig 单 turn 模式**：每个 turn 调用一次 rig `Agent::prompt`（不用 `multi_turn` 长循环）——输出指令后 turn 结束、**不阻塞等待子会话**；子会话 idle 或用户介入后再启动下一 turn；会话操作（创建会话、向子会话发指令、汇总）定义为 rig 工具
- **自动推进**：GUI 监听子会话状态（server 通知）；子 agent 会话变为 idle 时，系统自动向编排 agent 会话注入 prompt（含子会话完成情况），触发其评估结果并推进下一阶段
- **无独立状态机**：进展由编排 agent 会话内容与状态（idle / thinking）体现，用户自行判断
- **暂停 / 继续 / 介入**：均为向会话发送指令——暂停 / 继续发给编排 agent 会话由其控制子会话；介入可发给编排 agent 或子会话
- **持久化与恢复**：编排 agent 会话状态存 GUI 本地；GUI 关闭后自动推进停止（自动注入在 GUI 侧），子会话由各机器 server 继续运行；重开后依据子会话当前状态恢复

## 11. 参考

- [Agent Client Protocol (ACP) v1](https://agentclientprotocol.com/)：server 与 agent 之间的通信协议（stdio 传输、session 生命周期、session/update 事件流、request_permission）
- [GPUI](https://gpui.rs/)：Zed 的 GPU 加速 GUI 框架（Zed 主线 git 依赖）
- [gpui-component](https://github.com/longbridge/gpui-component)：GPUI 组件库（Dock 布局、Markdown、虚拟化列表、代码编辑器 + Tree Sitter、表单/对话框）
- [raft.build](https://raft.build)：Client-Server + WebSocket 的桌面应用架构参考
- [herdr](https://github.com/ogulcancelik/herdr)：终端 agent 多路复用，server 常驻与 attach/reattach 模式
- [t3code](https://github.com/pingdotgg/t3code)：agent 控制面——provider 驱动注册、按 turn 的 git checkpoint、事件溯源思路
