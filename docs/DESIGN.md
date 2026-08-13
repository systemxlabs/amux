# amux — 系统设计

**详略约定**：本文档只描述框架，不描述细节；实现细节交由实现 agent 决定。

---

## 1. 设计目标与范围

amux 定位为**跨机器 agent 控制平面**：GUI 应用统一调度多台机器上的 agent（Codex、Claude Code、Kimi Code），机器端由常驻 server 连接并驱动对应 agent。

本文档给出支撑 [PRD](PRD.md) 的系统级设计框架，覆盖：

- GUI 应用、各机器 server 的角色与边界
- GUI 应用 ↔ server 协议与传输
- 会话数据模型（消息、活动、历史存储）
- server 与 ACP agent 的交互
- 工作流（编排 agent）的运行机制
- 运行时生命周期、本地数据与可观测性

具体的内部模块划分、数据结构、错误处理策略等实现细节不在本文档范围内。

## 2. 术语

| 术语 | 含义 |
|---|---|
| **GUI 应用** | amux 桌面应用，客户端角色 |
| **Server** | 每台机器上的常驻进程 |
| **Agent** | 具体 agentic coding 工具（Codex / Claude / Kimi） |
| **ACP server** | agent 侧对外提供 ACP 服务的进程 |
| **普通会话** | 用户在指定机器上创建、由该机器 server 持有并驱动的会话 |
| **工作流会话** | 由 GUI 应用内置编排 agent 驱动的工作流会话，状态持久化于 GUI 应用本地 |
| **子会话** | 工作流所驱动的普通会话，挂载在工作流会话下 |
| **会话活动（activity）** | thinking / tool call / compaction 等过程事件聚合后的展示单元 |
| **ACP v1** | Agent Client Protocol，server 与 agent 之间的通信协议 |

## 3. 架构概览

Client-Server 架构：GUI 应用与各机器上的 server 常驻进程通过 WebSocket 通信；server 负责管理并对接（基于 ACP 协议）各 agent。

```
┌───────────────┐   WS(JSON-RPC)  ┌───────────────┐ ACP v1 (stdio) ┌────────────┐
│  GUI 应用     │◄───────────────►│ 机器 server    │◄──────────────►│ agent      │
│  ▲ 展示/聚合   │                 │ (常驻)         │  spawn 子进程   │ codex-acp  │
└───────────────┘                 │  ACP client   │                │ claude-acp │
                                  │  会话状态存储   │                │ kimi acp   │
                                  └───────────────┘                └────────────┘
```

### 3.1 组件与边界

- **GUI 应用（唯一客户端）**：amux 桌面应用（单进程，跨平台 macOS / Linux / Windows），直连各已注册机器的 server——本机与远程同等对待，统一注册后连接。每条连接对应一台机器，使用同一套协议。
- **Server**：每台机器运行一个常驻进程，是 **ACP v1 client**——spawn ACP server（子进程）、驱动 ACP 会话、把 agent 的会话事件**透传**给 GUI 应用、直连 git；同时是**会话控制面状态的权威维护者**（会话列表注册表 + 会话历史日志，见 §3.2）。**server 之间不通信**——每个 server 只服务本机会话，对连接方一律按 GUI 应用对待。

### 3.2 职责划分

- **会话列表与历史权威 = server**：会话列表由 server 本地**会话注册表**维护，元数据存于 server 本地；会话历史以 server 本地事件日志为权威。agent 的 ACP 会话仅承担执行（自身上下文恢复 `session/resume`）；**agent 侧存在但注册表未知的旧会话不被管理**（不列出、不打开、不回填）。
- **GUI 应用 = 聚合层**：会话列表由 GUI 应用汇总各 server（惰性加载：首次只取最近活跃会话，滚动加载更早）；跨机器工作流在 GUI 应用内部编排，不占用协议面；远程 server 离线时其会话标为不可达。
- **统一会话列表**：GUI 应用把各 server 返回的普通会话与 GUI 本地的工作流会话合并为一张统一列表，按最近活跃排序（工作流会话以其本地 `updated_at` 作为排序键）；子会话不进入顶层列表，仅随父会话展开显示。
- **机器身份**：机器名是 GUI 应用本地的用户别名（随机器注册表持久化），server 不自报机器名；server 由连接地址与 token 标识。
- **多设备共存**：任意数量的 GUI 应用可同时连接同一 server、查看并操作同一会话，互不踢出；各 GUI 应用独立加载会话数据；会话列表变更（创建 / 删除 / 重命名）经轮询同步，无专用变更通知。
- **生命周期解耦**：任何 GUI 应用断开（含 GUI 应用关闭）不停止 server、不销毁会话；会话仅由显式删除结束。

### 3.3 关键技术栈

- 异步运行时与网络：`tokio`、`tokio-tungstenite`（WebSocket）
- 序列化：`serde` / `serde_json`（JSON-RPC）
- 会话元数据持久化：SQLite（`rusqlite`，server 单写者场景，同步 API）
- ACP：`agent-client-protocol` 官方 SDK + `agent-client-protocol-tokio`
- 编排 agent LLM 抽象：`rig`
- GUI 应用：`gpui` + `gpui-component`

## 4. 部署与运行时

### 4.1 Server 生命周期

每台机器（含本机）统一运行一个 server 常驻进程（单二进制），与任何 GUI 应用连接无关：

- **启动**：server 由所在机器自行启动（手动命令、系统服务或安装脚本），GUI 应用不负责拉起——连接失败即视为该机器离线。
- **关闭**：GUI 应用关闭只断开 socket；server 继续常驻，ACP server 与 ACP 会话不受影响。
- **机器重启**：server 除会话注册表与历史日志外无持久化状态（配置可重建）——重启后**会话列表从本地注册表恢复**，历史日志在盘**直接权威使用**；ACP server **随 server 启动一起拉起**（发现与启动见 §7.3），普通会话（`session/new`）仍在首条 prompt 懒创建。
- **数据缺失容错**：历史日志缺失（损坏 / 清空）视为该会话历史为空；注册表丢失视为会话列表丢失。
- **GUI 应用视角**：本机与远程完全一致——注册、连接、认证、离线处理无差别。

### 4.2 GUI 应用生命周期

- **启动**：GUI 应用从本地数据目录加载机器注册表、快捷指令、Skills 注册表、工作流模板、编排 agent API 配置与工作流会话状态；按注册表逐个连接已注册的机器 server；连接失败或运行期断线则标记为离线并指数退避重连。
- **关闭**：只断开与各 server 的 WebSocket；server、ACP server 及进行中的会话均不受影响。重新打开后按注册表重新连接并恢复工作流会话的自动推进。

### 4.3 本地数据

- server 本地数据存放于 `~/.amux/server`：配置、**会话注册表**（SQLite 数据库 `amux.db`，存会话元数据，支持结构化查询与事务更新）与**会话历史存储**（`history/`，每会话一个事件日志文件）。
- GUI 应用本地数据存放于 `~/.amux/gui`：机器注册表（含各机器连接 token）、快捷指令、Skills 注册表、工作流模板、编排 agent API 配置、工作流会话状态。

## 5. 数据模型

会话数据指一个会话的全部内容：对话内容（用户消息 + agent 输出）与会话活动（activities）。本节统一描述它们的交付、存储与获取。

### 5.1 交付模型

- **事件透传**：agent 工作以 turn 为单位。server 把 agent 的 ACP `session/update` 事件（实时 turn）、`session_info_update`（agent 自报状态）与 **turn 边界事件**（server 从 prompt 请求生命周期反射：发出请求 = turn 开始，收到 result = turn 结束）**逐条透传**给 GUI 应用（typed JSON-RPC 通知），不做聚合。透传保持原始逐条事件（GUI 实时渲染需要流式）；落盘到本地历史日志的是**按 turn 合并后的条目**（§5.2），不存原始 chunk 流。
- **GUI 聚合与渲染**：对话内容（用户消息 + agent 输出）与活动（thinking / tool call / compaction）由 GUI 应用从透传事件聚合——输出按 chunk **增量实时渲染**，turn 结束收敛为完整消息；**同类连续事件合并为一条活动**（thinking 逐块累积为一条持续增长的思考活动，同一次工具调用的多次更新合并为一条）。
- **会话状态派生**：busy / idle 采信 `session_info_update`（agent 自报）并辅以 turn 边界，重连时经会话列表（meta 含 state）补齐。
- **断线重连**：server 不为断开的连接缓冲事件——断线期间进行中的 turn 不重放已产生的流式事件，重连后自接续收到的事件起增量渲染；已完成的 turn 经历史日志（§5.2）按需补齐。

### 5.2 会话历史

会话历史存于 server 本地事件日志（每会话一个事件日志文件，`~/.amux/server/history/`），**存储按 turn 合并后的条目**，GUI 应用按需读取：

- **权威与完整性**：server 是 ACP v1 唯一客户端（所有 prompt 经 server 串行化送达），是会话事件的唯一观察者；会话创建即建日志文件，此后**已完成 turn 的合并条目**按序追加，事件日志按设计保证完整（所有事件经 server 串行观察、单写者追加）
- **写入（合并落盘）**：turn 中缓冲流式事件（`session/update` chunk、thinking 块、工具调用、用户消息回显），**收到 result（turn 结束）时按 §5.1 的合并语义收敛为完整条目**后落盘（compaction 与 turn 边界独立成条）。
- **异常 turn**：崩溃（无 result）的 turn 视为未完成，不产生历史条目；**取消（cancel）同样以是否收到 result 为准**——agent 返回 result 则按合并语义落盘（保留已完成部分），否则视为未完成、不产生历史条目。
- **读取**：GUI 应用打开会话 → server 从本地日志读取（按窗口 / 游标惰性分页）——条目已合并，GUI 无需再按 chunk 收敛
- **会话删除**：删除会话时同步移除 server 注册表条目、删除历史日志，并释放 agent 侧资源，不可恢复

## 6. 传输与协议（GUI 应用 ↔ server）

### 6.1 WebSocket / JSON-RPC

- 传输统一为 **WebSocket**（tokio-tungstenite），消息格式为 **JSON-RPC 2.0**：请求必须回响应，事件以通知（无响应）表达。
- 单用户信任模型：无用户体系与权限系统，安全性依赖运行环境。

### 6.2 认证

- 所有连接统一携带 token。**本节的 token 指 server 自身的认证 token**：不落盘，每次启动由用户指定（`--token` 或环境变量 `AMUX_TOKEN`，统一名称 token，无别名），未指定则 server 拒绝启动。GUI 应用为每台已注册机器保存的连接 token 属于 GUI 本地配置（§4.3），会持久化以支持自动重连。

### 6.3 会话 ID

- server 直接颁发会话 ID（UUID，全局唯一由随机性保证）；机器归属由连接推断（每条连接对应一台机器），请求发给会话所属的 server 连接。

## 7. Server

### 7.1 会话交互

- **交互只有两个动作**：**prompt**（唯一消息入口：idle 启动新工作、忙时 steer；输入内容为文本 / 内嵌资源 / 资源引用）与 **cancel**（取消进行中的工作），经 ACP `session/prompt` / `session/cancel` 到达 agent。**prompt 是触发 ACP 交互的唯一入口**（创建会话只与 server 交互，见 §4.1）。
- **用户输入**：GUI 应用的 prompt 经 server 转发给 agent；用户消息同时由 GUI 应用本地立即渲染（不依赖回显），并保留在对话内容中。
- **快捷指令**（无专用协议）：每条指令是一段发给 agent 的提示词，经 prompt 由 agent 执行；新会话 / 取消 / 删除等由 GUI 应用直接发起对应会话操作。
- **多 GUI 应用并发**：server 对同一会话的所有 prompt（含各 GUI 应用的）按到达顺序串行化，保证按调用顺序送达。
- **忙时 prompt（steer）**：行为取决于 agent 实现（ACP turn 模型，见 §7.2）；agent 不支持进行中注入时 server 直接报错（不排队、不静默降级）。

### 7.2 与 ACP server 通信（ACP v1）

Server 作为 **ACP v1 client** 与各机器的 **ACP server** 通信：

- **传输**：ACP stdio——server spawn ACP server（子进程），JSON-RPC 2.0 over stdin/stdout。
- **会话生命周期**：
  - `session/new`：新建会话（yolo 模式启动，见下）
  - `session/resume`：对已存在会话恢复 agent 自身上下文（agent 从自身存储恢复，**不向客户端重放历史**——历史以 server 日志为权威，见 §5.2）
    - **能力门控**：agent 未声明 `sessionCapabilities.resume` 时无法恢复上下文，会话历史仍可浏览，用户 prompt 时报明确错误（不静默开新上下文）
  - `session/prompt` / `session/cancel`
  - `session/close`：删除会话时取消进行中工作并释放 agent 侧资源（见 §5.2）
- **状态与边界**：agent 经 `session_info_update` 自报状态；server 从 prompt 请求生命周期反射 turn 边界（发出请求 = turn 开始，收到 result = turn 结束）。
- **权限（yolo）**：agent 经 `session/request_permission` 请求权限；server **自动批准**（yolo 模式，既定决策延续，无审批往返），安全性依赖运行环境。
- **steer**：ACP v1 为 turn 模型——prompt 启动一个 turn，turn 结束（agent 回到 idle）后才可再 prompt；忙时 prompt 行为取决于 agent 实现（部分 agent 支持进行中注入）。

### 7.3 ACP server 发现与接入方式

ACP 接入方式分两类：

- **ACP 原生**：agent 原生支持 ACP，自带 ACP server（如 `kimi acp`）
- **adapter**：agent 不支持 ACP 时，经 **adapter**（适配器）把 agent CLI 暴露为 ACP server，间接支持 ACP

**每个 agent 的发现方式**：

| agent | 发现方式 | 接入 |
|---|---|---|
| kimi | PATH 上 `kimi` CLI 的 `acp` 子命令探测（`kimi acp --help` 命中） | 原生（`kimi acp`） |
| claude | 本机装有 `claude` CLI 且 npx 可用 → server 启动 `npx -y @agentclientprotocol/claude-agent-acp`，注册表中的 agent 名为 `claude` | adapter |
| codex | 本机装有 `codex` CLI 且 npx 可用 → server 启动 `npx -y @agentclientprotocol/codex-acp`，注册表中的 agent 名为 `codex` | adapter |

- 前置：server 所在机器需 node/npm（npx）；npx 首次运行会按需下载 adapter（需要网络）
- 认证（登录 / API key）由各 agent / adapter 自身管理，server 继承环境
- agent 发现并启动：server 启动时发现本机 agent 并**直接拉起**，无需用户手动指定；**启动失败的 agent 标记为不可用**，使用该 agent 时报明确错误，其余 agent 不受影响（server 照常启动）；运行期新发现的 agent 按需拉起。

### 7.4 Git 能力

server 直连本机 git，提供三类操作（GUI 经协议方法调用）：

- **status**：工作区文件列表 + 增减行数 + 分支；cwd 非 git 仓库时返回标记（GUI 不提供 diff 入口）
- **diff**：结构化 diff（按文件拆分，含每文件增减行数与 hunk 列表），供 side-by-side / inline 渲染
- **revert**：撤销工作区变更，支持单文件 / 单 hunk / 全部变更

### 7.5 Skills 管理

- **安装 / 更新**：复用会话能力——新建会话（对应机器与 agent）后经 prompt 让 agent 自行下载、安装或更新（快捷指令式 prompt，无专用协议）
- **查看已安装列表**：GUI 经 server 查询某 agent 已安装的 skills（agent 不支持时返回空列表）

## 8. 工作流

工作流由 **GUI 应用内置编排 agent** 驱动（rig 实现），基于会话原语实现：

### 8.1 运行模型

- **工作流会话**：工作流创建一个工作流会话（GUI 应用内置 agent，状态存 GUI 应用本地），与普通会话一样支持 prompt / 状态（idle / busy）；**支持 steer**——busy 时用户输入注入当前 turn（编排 agent 为 GUI 应用内置，steer 由 GUI 应用自身实现，不依赖 ACP 能力）。
- **rig 单 turn 模式**：每个 turn 调用一次 rig `Agent::prompt`（不用 `multi_turn` 长循环）——下达指令后 turn 结束、**不阻塞等待子会话**；下一 turn 由自动推进（§8.4）或用户输入触发。编排 agent 的会话操作定义为 rig 工具（§8.2）。
- **API 配置**：编排 agent 的 LLM 调用经自定义 API 端点配置——Base URL、API key、模型名与 **API format**：`chat_completions` / `responses` / `messages`（分别对应 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages API）。

### 8.2 编排工具

编排 agent 的工具即会话原语，经 GUI 应用既有的 server 连接与协议方法执行：

| 工具 | 用途 |
|---|---|
| `list_agents` | 已注册机器及各机器的 agent 列表：机器在线状态、agent 可用性——为每步选择机器与 agent |
| `list_sessions` | 本工作流的子会话列表（标题、状态、最近活跃）——复用已有子会话 |
| `create_session` | 在指定机器以指定 agent 与工作目录创建子会话，返回会话 ID |
| `prompt_session` | 向子会话下发指令（idle 启动工作、忙时 steer） |
| `cancel_session` | 取消子会话进行中的工作——卡住步骤的恢复手段 |
| `get_session_state` | 查询子会话当前状态（busy / idle、机器在线与否） |
| `read_session_history` | 按窗口 / 游标读取子会话对话内容——评估步骤结果、提取结论 |

汇总与结论不需要工具——编排 agent 的文本输出即工作流会话的对话内容。

### 8.3 上下文管理（auto compaction）

工作流会话长期运行、轮次持续增长，发送给模型的上下文（编排 agent 自身上下文，与 ACP agent 的 compaction 无关）需自动压缩：

- **历史与上下文分离**：展示用对话历史完整持久化（§4.3），压缩只影响发送给模型的上下文
- **触发**：每个 turn 开始前估算上下文 token 数，超过模型上下文窗口的阈值比例时先压缩再调用
- **压缩方式**：用编排 agent 自身模型把最旧的轮次压缩为一段摘要；此后上下文 = 系统提示词（含工作流模板）+ 摘要 + 未压缩的近期轮次
- **摘要持久化**：摘要随工作流会话状态落盘，GUI 重开后直接使用，不重新压缩

### 8.4 自动推进

工作流推进由 GUI 应用侧触发（server 不参与）：

- **触发源**：GUI 应用从透传事件派生子会话状态（§5.1），子会话 **busy → idle 跳变**（边沿触发）时向所属工作流会话注入一段 prompt
- **注入内容**：子会话标识（ID / 标题）+ turn 结束原因（完成 / 取消 / 错误，来自 prompt result）；步骤细节由编排 agent 经 `read_session_history` 按需自取，注入保持简短
- **busy 时注入**：注入即向工作流会话输入——idle 启动新 turn，busy 时作为 steer 注入当前 turn（§8.1）；多个子会话同时完成时合并为一次注入
- **恢复**：工作流会话状态记录最近一次自动注入时间；GUI 重开后，进行中工作流的子会话若最近活跃晚于该时间（完成未被消费），补注入一次——即「依据子会话当前状态恢复自动推进」

## 9. 可观测性

日志是 amux 调试的主要手段：GUI 应用 ↔ server ↔ ACP client ↔ agent 跨进程、跨机器，问题定位依赖能串起整条链路的日志。

## 10. 参考

- [Agent Client Protocol (ACP) v1](https://agentclientprotocol.com/)：server 与 agent 之间的通信协议（stdio 传输、session 生命周期、session/update 事件流、request_permission）
- [GPUI](https://gpui.rs/)：Zed 的 GPU 加速 GUI 应用框架（Zed 主线 git 依赖）
- [gpui-component](https://github.com/longbridge/gpui-component)：GPUI 组件库（Dock 布局、Markdown、虚拟化列表、表单/对话框）
- [raft.build](https://raft.build)：Client-Server + WebSocket 的桌面应用架构参考
- [herdr](https://github.com/ogulcancelik/herdr)：终端 agent 多路复用，server 常驻与 attach/reattach 模式
- [t3code](https://github.com/pingdotgg/t3code)：agent 控制面——provider 驱动注册、按 turn 的 git checkpoint、事件溯源思路
