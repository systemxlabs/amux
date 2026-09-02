# 系统设计文档

**详略约定**：本文档只描述框架，不描述实现细节，实现细节交由实现 agent 决定。

## 总体架构

Client-Server 架构：应用与各机器上的 Server 进程通过 WebSocket 通信；Server 负责管理并对接（基于 ACP 协议）各 agent。

![架构图](./amux-arch.drawio.png)

## Client-Server 通信

Client-Server 通信采用 WebSocket，消息格式为 **JSON-RPC 2.0**。

### 认证

每个 Server 启动时需要指定连接认证 Token，应用在机器注册阶段存储认证 Token，在应用与 Server 建立好 WS 连接后，应用必须发送如下认证消息
```json
{"jsonrpc":"2.0","method":"auth","params":{"token":"..."},"id":1}
```
Server 在未收到此认证消息并通过认证前，请求均返回认证失败响应。

### 协议

| 方法 | 描述 |
|---|---|
| `agent.list` | 查询当前机器的 agents，包含名称和可用性 |
| `agent.restart` | 重启指定 agent |
| `agent.rediscover` | 重新发现 agents |
| `session.new` | 新建一个普通会话 |
| `session.prompt` | 往指定普通会话发送指令 |
| `session.cancel` | 取消指定普通会话正在进行的工作 |
| `session.delete` | 删除指定普通会话 |
| `session.configure` | 配置指定普通会话：会话标题，会话选项等 |
| `session.config_options` | 获取指定普通会话的会话选项 |
| `session.slash_commands` | 获取指定普通会话的斜杠命令 |
| `session.plan` | 获取指定普通会话的 agent 计划 |
| `session.history` | 分页查询指定普通会话的对话历史 |
| `session.activities` | 分页查询指定普通会话的活动历史 |
| `session.ongoing_activity` | 查询指定普通会话正在进行中的活动 |
| `session.list` | 按数量查询最近活跃的普通会话列表 |
| `session.info` | 批量查询指定的普通会话列表 |
| `workspace.diff` | 查询普通会话工作目录改动 diff |
| `workspace.restore` | 可按文件或代码块撤销普通会话工作目录的改动 |
| `workspace.list` | 分页查看普通会话工作目录指定文件夹列表 |
| `workspace.read` | 分页查看普通会话工作目录指定路径文本文件内容 |
| `terminal.open` | 打开一个终端，指定 cwd 和 size 等，返回终端 ID |
| `terminal.resize` | 调整指定终端窗口大小 |
| `terminal.input` |  向指定终端输入内容 |
| `terminal.close` |  关闭指定终端 |

Server 主动推送通知
| 通知 | 描述 |
|---|---|
| `session.state_change` | 普通会话状态变更事件，包含变更原因 |
| `terminal.output` | 终端输出事件 |
| `terminal.exit` | 终端进程退出事件 |

## Server

### 技术栈

- 异步运行时与网络：`tokio`、`tokio-tungstenite`（WebSocket）
- 序列化：`serde` / `serde_json`（JSON-RPC）
- 会话元数据持久化：`rusqlite`
- ACP：`agent-client-protocol` 官方 SDK
- Git：`gitoxide` / git CLI
- PTY：`portable-pty`

### Server 启动

Server 启动和关闭由用户手动执行，启动参数包括
- `--host`: 监听地址，默认为 `0.0.0.0`
- `--port`: 监听端口，默认为 `34567`
- `--token`：指定认证 token，必传

### ACP Server 发现

Server 在启动阶段会自动从本机发现当前已安装的 Agent。

| agent | 发现方式 |
|---|---|
| kimi | 本机装有 `kimi` CLI 且 `kimi acp --help` 可用 |
| claude | 本机装有 `claude` CLI 且 npx 可用 |
| codex | 本机装有 `codex` CLI 且 npx 可用 |
| grok | 本机装有 `grok` CLI 且 `grok agent --help` 可用 |

### ACP Server 启动

Server 在启动阶段会同时通过子进程方式启动已发现的 ACP Server，通过 stdio 来与 ACP Server 通信。

| agent | 启动方式 |
|---|---|
| kimi | `kimi acp` |
| claude | `npx -y @agentclientprotocol/claude-agent-acp` |
| codex | `INITIAL_AGENT_MODE=agent-full-access npx -y @agentclientprotocol/codex-acp` |
| grok | `grok agent --always-approve stdio` |

### ACP Server 生命周期

Server 启动时会同时启动所有已安装的 ACP Servers，如果 ACP Server 启动失败，则标记不可用。

用户可从应用侧重启某一 ACP Server（无论是否已启动）。

Server 关闭时会同时关闭所有已启动的 ACP Servers，释放相应资源。

### ACP 通信

Server 作为 ACP client 与 ACP servers 通信
- 采用 ACP V1 协议通信
- 权限自动审批
- Client 能力支持：Terminal、Boolean Config Options
- 惰性创建新会话：用户创建会话时，仅在 Server 侧写入，等待用户发送指令或查询会话选项时，才向 ACP Server 发送 `session/new` 请求创建 agent 侧会话
- 惰性恢复已有会话：等待用户往已有会话发送指令或查询会话选项时，才向 ACP Server 发送 `session/resume` 请求恢复 agent 侧已有会话
- 设置会话选项：用户可基于当前会话可选项进行会话设置，Server 向 ACP Server 发送 `session/set_config_option` 请求进行设置
- 主动关闭长时间无活动会话：当会话长时间无活动（大于 1h）时，向 ACP Server 发送 `session/close` 请求关闭 agent 侧会话，释放资源
- 取消会话：当用户取消会话时，向 ACP Server 发送 `session/cancel` 请求来取消会话执行
- 删除会话：当用户删除会话时，如果会话已打开，向 ACP Server 发送 `session/close` 请求关闭 agent 侧会话，如果 ACP Server 支持会话删除，则发送 `session/delete` 请求删除 agent 侧会话
- 会话上下文大小：接收 ACP Server 的 `session/update` 通知的 `usage_update` 类型并记录会话的上下文窗口总大小和当前上下文大小

### 普通会话状态

普通会话状态以 Server 端会话元数据存储为权威，任何状态变更需立即落盘。

普通会话状态变更
- 新建会话时，会话状态为空闲
- Server 重启后，其上所有普通会话状态应置为空闲
- 当发送 `session/prompt` ACP 请求时，会话状态变为工作中
- 当接收 `session/prompt` ACP 响应时，会话状态变为空闲
- 当用户取消会话时，会话状态变为空闲

### 普通会话选项

普通会话选项存储在内存中，以 Agent 侧数据为权威
- 新建或恢复 ACP 会话时，存储其会话选项在内存中
- 当发送 `session/set_config_option` ACP 请求时，其响应中的会话选项全量覆盖内存存储
- 当接收 `session/update` ACP 通知的 `config_option_update` 类型时，其通知中的会话选项全量覆盖内存存储

### 普通会话斜杠命令

普通会话斜杠命令存储在内存中，以 Agent 侧数据为权威，当接收 `session/update` ACP 通知的 `available_commands_update` 类型时，其通知中的斜杠命令全量覆盖内存存储。

### 普通会话计划

普通会话计划存储在内存中，以 Agent 侧数据为权威，当接收 `session/update` ACP 通知的 `plan` 类型时，其通知中的计划全量覆盖内存存储。

### 普通会话删除

当用户请求删除普通会话时，立即从元数据中删除该普通会话，然后发起异步任务清理相关资源（如关闭或删除 agent 侧会话，清理关联的 worktree），随后返回响应。异步清理资源采用尽力而为的方式，不无限重试。

### 普通会话存储

普通会话数据包含三部分
- 元数据：存储在 `~/.amux/server/session.sqlite` 文件中，包含会话 ID、会话标题、会话状态、所属 Agent、Agent 会话 ID、工作目录、worktree 目录、上下文大小、上下文窗口总大小、最近活跃时间、创建时间等
- 对话历史：存储在 `~/.amux/server/sessions/<session_id>_history.jsonl` 文件中，仅包含用户输入和 agent 输出（agent 流式输出合并后写入）
- 活动历史：存储在 `~/.amux/server/sessions/<session_id>_activities.jsonl` 文件中，包含工具调用、thinking、执行错误等等（流式输出合并后写入）

### 工作树存储

Git worktree 统一存储在 `~/.amux/worktrees/<仓库目录名>-<随机串>/` 内。普通会话创建时若指定了 worktree 方式，则创建 worktree，普通会话被删除时，其关联的 worktree 也应一并删除。

当普通会话超过 7 天不活跃时，自动清理其关联的 worktree，后续可按需重建，保持路径一致。

### 终端

应用可创建一个或多个终端，终端不在多应用间共享，终端历史由应用侧维护，Server 侧终端存储在内存中。

终端与应用连接绑定，当应用与 Server 连接断开，其关联的终端资源被释放。

Server 发送终端事件时，仅向该终端关联的应用连接发送。

## 应用

### 机器连接

应用启动时会同时连接所有已注册的 Servers，根据连接是否成功标记机器状态为离线或在线。

应用会发送认证请求给每个已连接的 Server，若认证失败，则标记机器状态为认证失败。

用户可从应用侧手动重连某一 Server（无论是否已连接），无论机器离线或认证失败，应用不要自动重连。

应用关闭时会同时关闭所有 Server 连接。

### 会话列表

会话列表刷新机制
- 定时刷新：每隔 10s 刷新一次会话列表
- 主动刷新：当创建新会话、删除会话、重命名会话、用户或系统往会话发送用户消息时，主动触发会话列表刷新

会话列表滚动查询
1. 查询前 N 页工作流会话和各在线 Server 前 N 页普通会话
2. 若普通会话关联某工作流会话，但工作流会话不在本次查询集内，则从本次结果中过滤掉该普通会话
3. 根据工作流会话与普通会话集合，计算剩余需要查询的工作流会话关联的普通会话，批量向各 Server 查询
4. 根据查询到的结果集，按 UI 布局要求进行聚合展示

### 对话视图

会话的对话视图未打开时，不主动刷新，打开后才进行刷新。

对话消息刷新机制为
- 定时刷新：每隔 10s 刷新一次，如有新增对话消息，增量渲染
- 主动刷新：当用户输入消息后，主动触发刷新，此时新增用户输入消息，增量渲染

实时活动刷新机制为每隔 2s 刷新一次。

### 活动视图

会话的活动视图未打开时，不主动刷新。打开后，采用定时刷新机制，每隔 10s 刷新一次，如有新增活动，增量渲染。

### 编排智能体

系统提示词应包括
- 角色，工作方式，行为约束
- 工作流计划

| 工具 | 用途 |
|---|---|
| `list_agents` | 已注册机器及各机器的 agent 列表：机器状态、agent 状态等 |
| `list_sessions` | 本工作流的关联普通会话列表，会话包含标题、状态等尽可能多的信息 |
| `create_session` | 创建关联普通会话 |
| `prompt_session` | 向指定关联普通会话下发指令 |
| `cancel_session` | 取消指定关联普通会话进行中的工作 |
| `configure_session` | 配置指定关联普通会话：会话标题，会话选项等 |
| `get_session_config_options` | 获取指定关联普通会话的会话选项 |
| `read_session_history` | 分页读取关联普通会话对话内容 |
| `read_session_activities` | 分页读取关联普通会话活动内容 |

编排智能体实现应支持 steer，当工作流会话处于工作中时，接收的用户消息以 steer 方式注入。

### 工作流会话驱动

当收到关联普通会话的工作中->空闲且变更原因非取消的状态变更事件时，系统往工作流会话以用户消息方式注入如下内容

> 关联普通会话 `<session_id>@<机器名称>` 检测到状态变更：<旧状态> -> <新状态>，变更原因为 <变更原因>

### 工作流会话取消

用户可点击取消按钮，系统往工作流会话以用户消息方式注入如下内容

> 取消当前工作流会话关联的所有普通会话，停止工作流调度

用户可打字输入指令来指示编排智能体如何取消工作流会话及其关联普通会话。

### 工作流会话存储

工作流会话数据包含三部分
- 元数据：存储在 `~/.amux/app/session.sqlite` 文件中，包含会话 ID、会话标题、会话状态、执行计划、最近活跃时间、关联普通会话等
- 对话历史：存储在 `~/.amux/app/sessions/<session_id>_history.jsonl` 文件中，仅包含用户输入和编排智能体输出（流式输出合并后写入）
- 活动历史：存储在 `~/.amux/app/sessions/<session_id>_activities.jsonl` 文件中，包含工具调用、thinking、执行错误等等（流式输出合并后写入）

### 工作流计划存储

存储在 `~/.amux/app/workflows.json` 路径，格式为
```json
[
  { "name": "amux开发工作流", "plan": "xxx" }
]
```
注意 name 必须唯一。

### 注册机器存储

存储在 `~/.amux/app/machines.json` 路径，格式为
```json
[
  { "name": "localpc", "url": "ws://127.0.0.1:3457", "token": "xxx" }
]
```
注意 name 必须唯一。

### 技能存储

存储在 `~/.amux/app/skills.json` 路径，格式为
```json
[
  { "name": "opencli", "description": "位于 https://github.com/jackwener/OpenCLI/tree/main/skills，包含多个 skills" }
]
```
注意 name 必须唯一。

### 常用工作目录存储

存储在 `~/.amux/app/recent_workspaces.json` 路径，格式为
```json
[
  { "machine": "localpc", "workspace": "/home/linwei/workspace/amux", "lastUsed": 1729000000000 }
]
```
注意 (machine, workspace) 组合必须唯一。

### 快捷指令存储

存储在 `~/.amux/app/quick_commands.json` 路径，格式为
```json
[
  {
    "name": "Commit & Push",
    "prompt": "提交并推送当前工作区的更改：为改动写一条简洁的 commit message，commit 后 push。"
  },
  {
    "name": "Submit PR",
    "prompt": "提交一个 Pull Request：stage → commit → push → 创建 PR。"
  }
]
```
注意 name 必须唯一。

### 编排智能体配置存储

存储在 `~/.amux/app/agent.json` 路径，格式为
```json
{
  "apiFormat": "responses",
  "baseUrl": "https://api.deepseek.com/v1",
  "apiKey": "sk-xxx",
  "model": "deepseek-v4-flash"
}
```

### 桌面应用

#### 技术栈

- GUI：`gpui` + `gpui-component`
- 编排智能体：`rig`
- 终端：`alacritty_terminal`

### Web 应用
待定

## 可观测性

应用和 Server 在实现时，均需埋点丰富的日志。日志按天切片，存储最近 7 天的日志。

日志级别默认为 info，依赖库日志级别默认为 warn，支持通过 RUST_LOG 环境变量调整。

日志库采用 `logforth`。

日志存储
- 桌面应用：存放在 `~/.amux/logs/desktop.log`
- Server：存放在 `~/.amux/logs/server.log`

## 参考

- [Agent Client Protocol (ACP) v1](https://agentclientprotocol.com/)：server 与 agent 之间的通信协议
- [GPUI](https://gpui.rs/)：Zed 的 GPU 加速 GUI 应用框架
- [gpui-component](https://github.com/longbridge/gpui-component)：GPUI 组件库
- [raft.build](https://raft.build)：Client-Server + WebSocket 的桌面应用架构参考