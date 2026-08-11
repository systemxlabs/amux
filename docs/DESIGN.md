# amux — 系统设计

**详略约定**：本文档只描述框架，不描述细节；实现细节交由实现 agent 决定。

---

## 1. 设计目标与范围

amux 定位为**跨机器 agent 控制平面**：GUI 应用统一调度多台机器上的 agent（Codex、Claude Code、Kimi Code），机器端由常驻 server 连接并驱动对应 agent。

本文档给出支撑 [PRD](PRD.md) 的系统级设计框架，覆盖：

- GUI 应用、各机器 server 的角色与边界
- GUI 应用 ↔ server 协议与传输
- 会话数据模型（消息、活动、历史、缓存）
- server 与 ACP agent 的交互
- 工作流（编排 agent）的运行机制
- 运行时生命周期、本地数据与可观测性

具体的内部模块划分、数据结构、错误处理策略等实现细节不在本文档范围内。

## 2. 术语

| 术语 | 含义 |
|---|---|
| **GUI 应用（客户端 / 桌面应用）** | amux 桌面应用（GPUI），客户端角色，全文统一称 GUI 应用 |
| **Server** | 每台机器上的常驻进程，作为 ACP v1 client 与 agent 通信 |
| **Agent** | 具体 agentic coding 工具（Codex / Claude / Kimi）的 ACP server 端 |
| **agent 会话** | 用户在指定机器上创建、由该机器 server 持有并驱动的普通会话 |
| **编排 agent 会话 / 工作流会话** | 由 GUI 应用内置编排 agent 驱动的工作流会话，状态持久化于 GUI 应用本地 |
| **子会话** | 工作流所驱动的 agent 会话，挂载在工作流会话下 |
| **会话活动（activity）** | thinking / tool call / compaction 等过程事件聚合后的展示单元 |
| **ACP v1** | Agent Client Protocol，server 与 agent 之间的通信协议 |

## 3. 架构概览

Client-Server 架构：GUI 应用（**GPUI**）与各机器上的 server 常驻进程通过 WebSocket 通信；server 作为 **ACP v1 client** 对接各 agent（Codex / Claude / Kimi），经 ACP 的 stdio 传输 spawn agent 子进程。

```
┌──────────────┐   WS(JSON-RPC)  ┌───────────────┐  ACP v1 (stdio) ┌────────────┐
│  GUI 应用     │◄───────────────►│ 机器 server   │◄───────────────►│ agent      │
│ (GPUI)       │                 │ (常驻)         │   spawn 子进程   │ codex-acp  │
│  ▲           │                 │  ACP client    │                │ claude-acp │
│  │ 会话历史    │                 └───────────────┘                │ kimi acp   │
│  └ 本地缓存   │                                                    └────────────┘
└──────────────┘
```

### 3.1 组件与边界

- **GUI 应用（唯一客户端）**：amux 桌面应用（GPUI），直连各已注册机器的 server——本机与远程同等对待，统一注册后连接。每条连接对应一台机器，使用同一套协议。
- **Server**：每台机器运行一个常驻进程，是 **ACP v1 client**——spawn agent 子进程、驱动 ACP 会话、把 agent 的会话事件**透传**给 GUI 应用、直连 git。**server 之间不通信**——每个 server 只服务本机会话，对连接方一律按 GUI 应用对待。
- **协议单一来源**：GUI 应用 ↔ server 协议（方法面、参数/结果类型、通知类型）由共享 crate 定义，GUI 应用与 server 从同一处导入——单一语言实现，无需双语言协议对齐。

### 3.2 职责划分

- **历史权威 = agent**：会话历史的唯一真相源在 agent 侧；server 不保存。
- **GUI 应用 = 聚合层**：会话列表由 GUI 应用汇总各 server（server 从 agent `session/list` 获得）；跨机器工作流是 GUI 应用内部编排，基于会话原语实现，不占用协议面；远程 server 离线时其会话标为不可达。
- **多设备共存**：任意数量的 GUI 应用可同时连接同一 server、查看并操作同一会话，互不踢出；各 GUI 应用独立加载与缓存会话数据。
- **生命周期解耦**：任何 GUI 应用断开（含 GUI 应用关闭）不停止 server、不销毁会话；会话仅由显式关闭 / 删除结束。

### 3.3 关键技术栈

- 异步运行时与网络：`tokio`、`tokio-tungstenite`（WebSocket）
- 序列化：`serde` / `serde_json`（JSON-RPC）
- ACP：`agent-client-protocol` 官方 SDK + `agent-client-protocol-tokio`
- 编排 agent LLM 抽象：`rig`
- GUI 应用：`gpui` + `gpui-component`

## 4. 部署与运行时

### 4.1 Server 生命周期与启动

每台机器（含本机）统一运行一个 server 常驻进程（单二进制），与任何 GUI 应用连接无关：

- **启动**：server 由所在机器自行启动（手动命令、系统服务或安装脚本），GUI 应用不负责拉起——连接失败即视为该机器离线；认证 token 每次启动需指定。
- **关闭**：GUI 应用关闭只断开 socket；server 继续常驻，agent 子进程与 ACP 会话不受影响。
- **机器重启**：server 无持久化状态（历史在 agent 侧、配置可重建）——重启后重新 spawn agent 子进程，会话列表经 ACP `session/list` 从 agent 侧恢复。
- **GUI 应用视角**：本机与远程完全一致——注册、连接、认证、离线处理无差别。

### 4.2 GUI 应用生命周期

- **启动**：GUI 应用从本地数据目录加载机器注册表、快捷指令、Skills 注册表、工作流模板与编排 agent 会话状态；按注册表逐个连接已注册的机器 server；连接失败则标记为离线并指数退避重连。
- **关闭**：只断开与各 server 的 WebSocket；server、agent 子进程及进行中的会话均不受影响。重新打开后按注册表重新连接并恢复编排 agent 会话的自动推进。

### 4.3 本地数据

- server 其余本地数据存放于 `~/.amux/server`（配置等）；**认证 token 不落盘**。
- GUI 应用本地数据存放于 `~/.amux/gui`：机器注册表、快捷指令、Skills 注册表、会话历史缓存、编排 agent 会话状态。

## 5. 数据模型

会话数据指一个会话的全部内容：对话内容（用户消息 + agent 输出）与会话活动（activities）。本节统一描述它们的交付、存储与获取。

### 5.1 交付模型

- **事件透传**：agent 工作以 turn 为单位。server 把 agent 的 ACP `session/update` 事件（load 重放与实时 turn）、`session_info_update`（agent 自报状态）与 **turn 边界事件**（server 从 prompt 请求生命周期反射：发出请求 = turn 开始，收到 result = turn 结束）**逐条透传**给 GUI 应用（typed JSON-RPC 通知），不做聚合。
- **GUI 聚合与状态**：对话内容（用户消息 + agent 输出）、活动（thinking / tool call / compaction）与会话状态（忙 / 就绪）均由 GUI 应用从透传事件聚合 / 派生——输出按 chunk **增量实时渲染**，turn 结束收敛为完整消息；busy / idle 采信 `session_info_update`（agent 自报）并辅以 turn 边界，重连时经会话列表（meta 含 state）补齐。

### 5.2 会话历史

会话历史**按需拉取**：GUI 应用建立连接时只获取会话列表，**不拉取任何会话历史**；点击某个会话查看时，才经 ACP `session/load` 全量重放并聚合为对话内容，缓存到本地（GUI 应用数据目录，按会话一个缓存文件；`session/load` 的响应即重放边界）：

- **查看会话**：点击会话 → 重放事件聚合为对话内容 → 写入本地缓存（仅查看期间）
- **增量**：turn 中实时事件由 GUI 应用聚合追加
- **会话删除**（ACP `session/delete`）：历史随 agent 侧删除而消失，不可恢复
- 缓存仅作会话查看期间的读写（滚动回溯、分页），不承担历史权威

### 5.3 会话活动

- 活动（thinking / tool call / compaction 等）由 GUI 应用从透传事件聚合；**同类连续事件合并为一条**（thinking 逐块累积、同一工具调用合并）。
- **实时活动**：turn 进行中，GUI 应用实时聚合当前活动，展示于实时活动条与活动视图，空闲时清空。
- **历史活动**：GUI 应用在活动视图需要时从 load 重放与实时事件聚合（server 不缓存活动）。

## 6. 传输与协议（GUI 应用 ↔ server）

### 6.1 WebSocket / JSON-RPC

- 传输统一为 **WebSocket**（tokio-tungstenite），消息格式为 **JSON-RPC 2.0**：请求必须回响应，事件以通知（无响应）表达。
- 单用户信任模型：无用户体系与权限系统，安全性依赖运行环境。

### 6.2 认证

- 所有连接统一携带 token；**token 不落盘，每次启动由用户指定**（`--token` 或环境变量 `AMUX_TOKEN`，统一名称 token，无别名），未指定则 server 拒绝启动。

### 6.3 会话 ID 与重连

- **会话 ID**：server 直接颁发全局唯一的会话 ID；机器归属由连接推断（每条连接对应一台机器），请求发给会话所属的 server 连接。
- 断线后 GUI 应用指数退避重连。

## 7. Server

### 7.1 会话交互

- 交互只有两个动作：**prompt**（唯一消息入口：idle 启动新工作、忙时 steer；输入内容为文本 / 内嵌资源 / 资源引用）与 **cancel**（取消进行中的工作），经 ACP `session/prompt` / `session/cancel` 到达 agent。
- **用户输入**：GUI 应用的 prompt 经 server 转发给 agent；用户消息同时由 GUI 应用本地立即渲染（不依赖回显），并保留在对话内容中。
- **快捷指令**（GUI 应用本地配置，无专用协议）：每条指令是一段发给 agent 的提示词，经 prompt 由 agent 执行（Commit & Push、Submit PR、skill 安装 / 更新等，见「Skills 管理」）；直连 git 的 push / undo / revert 等操作不属于快捷指令；新会话 / Kill Session 由 GUI 应用直接发起对应会话操作。
- **多 GUI 应用并发**：server 对同一会话的所有 prompt（含各 GUI 应用的）按到达顺序串行化，保证按调用顺序送达。
- **忙时 prompt（steer）**：行为取决于 agent 实现（ACP turn 模型，见 §7.2）；agent 不支持进行中注入时 server 直接报错（不排队、不静默降级）。

### 7.2 与 Agent 通信（ACP v1）

Server 作为 **ACP v1 client**（依赖官方 SDK `agent-client-protocol`）与各 agent 通信：

- **传输**：ACP stdio——server spawn agent 子进程（`codex-acp` / `claude-acp` / `kimi acp`），JSON-RPC 2.0 over stdin/stdout。
- **会话生命周期**：
  - `session/new`：新建会话（yolo 模式启动，见下）
  - `session/load`：加载会话并**全量重放历史**（`session/update` 通知流，重放完才响应）
  - `session/prompt` / `session/cancel` / `session/delete` / `session/list`
- **状态与边界**：agent 经 `session_info_update` 自报状态；server 从 prompt 请求生命周期反射 turn 边界（发出请求 = turn 开始，收到 result = turn 结束）。
- **权限（yolo）**：agent 经 `session/request_permission` 请求权限；server **自动批准**（yolo 模式，既定决策延续，无审批往返），安全性依赖运行环境。
- **steer**：ACP v1 为 turn 模型——prompt 启动一个 turn，turn 结束（agent 回到就绪）后才可再 prompt；忙时 prompt 行为取决于 agent 实现（部分 agent 支持进行中注入）。

### 7.3 Agent 发现与接入方式

ACP 接入方式分两类：

- **ACP 原生**：agent 自带 ACP 服务器（如 `kimi acp`）
- **ACP 包装器**：agentclientprotocol 官方包装器，把不支持 ACP 的 CLI 暴露为 ACP 服务器；server 经 **npx 直接运行**（`npx -y @agentclientprotocol/codex-acp` 等），无需手动安装

**每个 agent 的发现方式**：

| agent | 发现方式 | 接入 |
|---|---|---|
| kimi | PATH 上 `kimi` CLI 的 `acp` 子命令探测（`kimi acp --help` 命中） | ACP 原生（`kimi acp`） |
| claude | 本机装有 `claude` CLI 且 npx 可用 → server 启动 `npx -y @agentclientprotocol/claude-agent-acp`，harness 名映射为 `claude` | ACP 包装器 |
| codex | 本机装有 `codex` CLI 且 npx 可用 → server 启动 `npx -y @agentclientprotocol/codex-acp`，harness 名映射为 `codex` | ACP 包装器 |

- 前置：server 所在机器需 node/npm（npx）；npx 首次运行会按需下载包装器（需要网络）
- 认证（登录 / API key）由各包装器/CLI 自身管理，server 继承环境
- 包装器按需懒加载：首次实际连接时才启动（npx 下载），发现阶段仅校验 CLI 与 npx 可用

## 8. GUI 应用（GPUI）

GUI 应用为单进程桌面应用（GPUI + gpui-component；跨平台 macOS / Linux / Windows）。

### 8.1 技术栈与异步模型

- **UI 框架**：GPUI executor 承载 UI；WS 连接与 ACP 重放流经 **tokio** 运行，事件桥接进 GPUI 事件循环。

### 8.2 布局与关键视图

- **布局**：三面板 **Dock 布局**；右侧上下文面板（diff / 会话详情 / 会话活动）展开时**窗口向右扩展**，不压缩中间面板空间，关闭时收回。
- **对话流**：只展示用户消息与 agent 输出的消息气泡（**Markdown 渲染**）+ 虚拟化列表；输出**实时流式渲染**（增量追加），turn 结束收敛为完整消息；**气泡标注 agent 与所属机器**（`agent@机器`），编排会话气泡标注「编排」。
- **会话活动**：中间面板下方展示**正在进行的活动**（一条或无，实时）；右侧面板展示完整活动历史（上下滚动）——来自 GUI 应用对重放与实时事件的聚合。
- **Diff Review**：代码编辑器组件 + **Tree Sitter 语法高亮**；文件列表、side-by-side/inline diff、revert 操作。
- **输入与设置**：输入区（多行、拖拽/粘贴、@ 引用）、快捷指令栏、设置页（机器管理）——表单/对话框组件。

## 9. 工作流

工作流由 **GUI 应用内置编排 agent** 驱动（**rig 单 turn 模式**实现），基于会话原语实现，不占用协议面：

- **编排 agent 会话**：工作流创建一个编排 agent 会话（GUI 应用内置 agent，状态存 GUI 应用本地），与普通会话一样支持 prompt / 状态（idle / thinking）。
- **rig 单 turn 模式**：每个 turn 调用一次 rig `Agent::prompt`（不用 `multi_turn` 长循环）——输出指令后 turn 结束、**不阻塞等待子会话**；子会话 idle 或用户介入后再启动下一 turn；会话操作（创建会话、向子会话发指令、汇总）定义为 rig 工具。
- **API 配置校验**：创建编排会话前校验编排 agent 配置，缺失时 GUI 应用提示并引导到设置页、不创建不可用会话；运行期 LLM 调用失败把错误作为 System 消息写入编排会话对话历史（随持久化保留），会话回到 idle 而非假忙。
- **自动推进**：GUI 应用监听子会话状态（从透传事件派生）；子 agent 会话变为 idle 时，系统自动向编排 agent 会话注入 prompt（含子会话完成情况），触发其评估结果并推进下一阶段。
- **无独立状态机**：进展由编排 agent 会话内容与状态（idle / thinking）体现，用户自行判断。
- **暂停 / 继续 / 介入**：均为向会话发送指令——暂停 / 继续发给编排 agent 会话由其控制子会话；介入可发给编排 agent 或子会话。
- **持久化与恢复**：编排 agent 会话状态存 GUI 应用本地；GUI 应用关闭后自动推进停止（自动注入在 GUI 应用侧），子会话由各机器 server 继续运行；重开后依据子会话当前状态恢复。

## 10. 可观测性

日志是 amux 调试的主要手段：GUI 应用 ↔ server ↔ ACP client ↔ agent 跨进程、跨机器，问题定位依赖能串起整条链路的日志。

## 11. 参考

- [Agent Client Protocol (ACP) v1](https://agentclientprotocol.com/)：server 与 agent 之间的通信协议（stdio 传输、session 生命周期、session/update 事件流、request_permission）
- [GPUI](https://gpui.rs/)：Zed 的 GPU 加速 GUI 应用框架（Zed 主线 git 依赖）
- [gpui-component](https://github.com/longbridge/gpui-component)：GPUI 组件库（Dock 布局、Markdown、虚拟化列表、代码编辑器 + Tree Sitter、表单/对话框）
- [raft.build](https://raft.build)：Client-Server + WebSocket 的桌面应用架构参考
- [herdr](https://github.com/ogulcancelik/herdr)：终端 agent 多路复用，server 常驻与 attach/reattach 模式
- [t3code](https://github.com/pingdotgg/t3code)：agent 控制面——provider 驱动注册、按 turn 的 git checkpoint、事件溯源思路
