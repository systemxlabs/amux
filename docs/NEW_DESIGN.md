# 系统设计文档

**详略约定**：本文档只描述框架，不描述实现细节，实现细节交由实现 agent 决定。

## 总体架构

采用三级架构：Daemon 常驻在各个机器上，Server 常驻在公网服务器上，应用运行在客户端。Server 与各机器上的 Daemon 进程通过 WebSocket 通信，应用与 Server 进行 HTTPS 通信。

TODO 架构图

## 通信

### Server-Daemon 通信

Server-Daemon 通信采用 WebSocket，消息格式为 JSON-RPC 2.0。

#### 认证

在 Daemon 与 Server 建立 WebSocket 连接握手期间，Daemon 发送的握手请求需携带头部 `Authorization: Bearer <token>` 和 `amux-machine: <machine_name>`，Server 需要校验 token 是否正确，如不正确则握手失败。

#### 协议

| 方法 | 描述 |
|---|---|
| `agent.discover` | 发现当前机器已安装的 agents |
| `agent.restart` | 重启指定 agent |
| `git.diff` | 查询指定仓库改动 diff |
| `git.restore` | 可按文件或代码块撤销指定仓库的改动 |
| `git.worktree.new` | 从指定仓库创建一个 worktree |
| `git.worktree.resume` | 从指定仓库指定路径恢复 worktree |
| `git.worktree.list` | 查询指定仓库所有 worktrees |
| `git.worktree.remove` | 删除指定 worktree |
| `fs.list` | 分页查看指定路径文件夹列表 |
| `fs.read` | 分页查看指定路径文本文件内容 |
| `terminal.open` | 打开一个终端，指定 cwd 和 size 等，返回终端 ID |
| `terminal.resize` | 调整指定终端窗口大小 |
| `terminal.input` |  向指定终端输入内容 |
| `terminal.close` |  关闭指定终端 |

Daemon 主动推送通知
| 通知 | 描述 |
|---|---|
| `acp` | ACP 消息转发 |
| `terminal.output` | 终端输出事件 |
| `terminal.exit` | 终端进程退出事件 |

Server 主动推送通知
| 通知 | 描述 |
|---|---|
| `acp` | ACP 消息转发 |

### Client-Server 通信

Client-Server 通信采用 HTTPS。

#### 认证

Client 向 Server 发送请求时，其头部必须携带 `Authorization: Bearer <token>`，Server 对每个 Client 请求都需要进行验证。

#### 协议

| 方法 | 描述 |
|---|---|
| GET `/machines` | 查询所有机器 |
| GET `/machines/<machine_name>/agents` | 查询当前机器的 agents，包含名称和可用性 |
| POST `/machines/<machine_name>/agents/rediscover` | 重新发现 agents |
| POST `/machines/<machine_name>/agents/<agent_name>/restart` | 重启指定 agent |
| GET `/machines/<machine_name>/list_dir` | 分页查看指定路径文件夹列表 |
| GET `/machines/<machine_name>/read_file` | 分页查看指定路径文本文件内容 |
| POST `/machines/<machine_name>/terminals` | 打开一个终端，指定 cwd 和 size 等，返回终端 ID |
| GET `/machines/<machine_name>/terminals` | 查询所有打开的终端 |
| POST `/machines/<machine_name>/terminals/<terminal_id>` | 向指定终端输入内容 |
| GET `/machines/<machine_name>/terminals/<terminal_id>` | 读取指定终端输出内容 |
| DELETE `/machines/<machine_name>/terminals/<terminal_id>` | 关闭指定终端 |
| POST `/machines/<machine_name>/terminals/<terminal_id>/resize` | 调整指定终端窗口大小 |
| POST `/sessions` | 新建一个普通会话 |
| GET `/sessions` | 分页查询最近活跃的普通会话列表 |
| GET `/sessions/<session_id>` | 查询指定普通会话 |
| POST `/sessions/<session_id>` | 往指定普通会话发送指令 |
| DELETE `/sessions/<session_id>` | 删除指定普通会话 |
| POST `/sessions/<session_id>/cancel` | 取消指定普通会话 |
| POST `/sessions/<session_id>/configure` | 配置指定普通会话：会话标题，会话选项等 |
| GET `/sessions/<session_id>/config_options` | 获取指定普通会话的会话选项 |
| GET `/sessions/<session_id>/slash_commands` | 获取指定普通会话的斜杠命令 |
| GET `/sessions/<session_id>/plan` | 获取指定普通会话的 agent 计划 |
| GET `/sessions/<session_id>/context` | 获取指定普通会话的上下文信息 |
| GET `/sessions/<session_id>/history` | 分页查询指定普通会话的对话历史 |
| GET `/sessions/<session_id>/activities` | 分页查询指定普通会话的活动历史 |
| GET `/sessions/<session_id>/ongoing_activity` | 查询指定普通会话正在进行中的活动 |
| GET `/sessions/<session_id>/diff` | 查询普通会话工作目录改动 diff |
| POST `/sessions/<session_id>/restore` | 可按文件或代码块撤销普通会话工作目录的改动 |
| POST `/workflows` | 新建一个工作流会话 |
| GET `/workflows` | 分页查询最近活跃的工作流会话列表 |
| GET `/workflows/<workflow_id>` | 查询指定工作流会话 |
| POST `/workflows/<workflow_id>` | 往指定工作流会话发送指令 |
| DELETE `/workflows/<workflow_id>` | 删除指定工作流会话 |
| POST `/workflows/<workflow_id>/cancel` | 取消指定工作流会话 |
| POST `/workflows/<workflow_id>/configure` | 配置指定工作流会话：会话标题等 |
| GET `/workflows/<workflow_id>/history` | 分页查询指定普通会话的对话历史 |
| GET `/workflows/<workflow_id>/activities` | 分页查询指定普通会话的活动历史 |
| GET `/workflows/<workflow_id>/ongoing_activity` | 查询指定普通会话正在进行中的活动 |

## Daemon

Daemon 常驻于每个机器上，主要负责 ACP Servers 多路复用、生命周期管理和执行与机器绑定的功能。

### 技术栈

- 基础库：`tokio` / `serde` / `serde_json`
- WebSocket：`tokio-tungstenite`
- ACP：`agent-client-protocol` 官方 SDK
- Git：`gitoxide` / git CLI
- PTY：`portable-pty`
- CLI: `clap`

### Daemon 启动

Daemon 启动和关闭由用户手动执行，启动参数包括
- `--machine`：机器名称
- `--server`: Server 的 WebSocket 地址
- `--token`：认证 token

### ACP Server 发现

Server 会发送指令让 Daemon 从本机发现当前已安装的 Agent。

| agent | 发现方式 |
|---|---|
| codex | 本机装有 `codex` CLI 且 npx 可用 |

### ACP Server 启动

Server 会发送指令让 Daemon 启动某个 ACP Server。

| agent | 启动方式 |
|---|---|
| codex | `INITIAL_AGENT_MODE=agent-full-access npx -y @nyssance/codex-acp-v2` |

### ACP Server 生命周期

Server 会发送指令让 Daemon 启动某个 ACP Servers，如果 ACP Server 启动失败，则返回错误。

Server 可重启某一 ACP Server（无论是否已启动）。

Daemon 关闭时会同时关闭所有已启动的 ACP Servers，释放相应资源。

### ACP Server 多路复用

Daemon 本身不与 ACP Server 进行任何通信，只作为 Server 与 ACP Servers 之间的桥梁进行消息转发，由于共享一个 WebSocket 连接，需要进行多路复用，转发的 ACP 消息格式如下
```json
{
  "jsonrpc": "2.0", 
  "method": "acp", 
  "params": {
    "agent": "xxx",
    "raw": "..."
  }
}
```

### 工作树存储

Git worktree 统一存储在 `~/.amux/worktrees/<仓库目录名>-<随机串>/` 内。Git worktree 生命周期由 Server 进行管理。


### 终端存储

Daemon 在内存中存储终端元信息，终端历史由 Server 侧维护。当 Daemon 与 Server 连接断开，其关联的终端资源被释放。

### 自动重连

当 Server 不在线或连接断开，每隔 1 分钟重连一次。

## Server

### 技术栈

- 基础库：`tokio` / `serde` / `serde_json`
- WebSocket：`tokio-tungstenite`
- SQLite：`rusqlite`
- ACP：`agent-client-protocol` 官方 SDK
- Git：`gitoxide` / git CLI
- PTY：`portable-pty`
- CLI: `clap`

### Server 启动

Server 启动和关闭由用户手动执行，启动参数包括
- `--host`: 监听地址，默认为 `0.0.0.0`
- `--port`: 监听端口，默认为 `34567`
- `--token`：指定认证 token，必传

### Daemon 连接

当 Daemon 与 Server 建立好连接后，Server 应发送命令查询 Daemon 机器已安装 agents 并并行启动已发现的 ACP Server，然后通过 Daemon 与已启动的 ACP Server 建立 ACP 连接和初始化。

### ACP 通信

Server 作为 ACP client 与 ACP servers 通信
- 采用 ACP V2 协议通信，不支持 V1 协议
- 权限自动审批
- Client 能力支持：空
- 惰性创建新会话：用户创建会话时，仅在 Server 侧写入，等待用户发送指令或查询会话选项时，才向 ACP Server 发送 `session/new` 请求创建 agent 侧会话
- 惰性恢复已有会话：等待用户往已有会话发送指令或查询会话选项时，才向 ACP Server 发送 `session/resume` 请求恢复 agent 侧已有会话
- 设置会话选项：用户可基于当前会话可选项进行会话设置，Server 向 ACP Server 发送 `session/set_config_option` 请求进行设置
- 主动关闭长时间无活动会话：当会话长时间无活动（大于 1h）时，向 ACP Server 发送 `session/close` 请求关闭 agent 侧会话，释放资源
- 取消会话：当用户取消会话时，向 ACP Server 发送 `session/cancel` 通知来取消会话执行
- 删除会话：当用户删除会话时，如果会话已打开，向 ACP Server 发送 `session/close` 请求关闭 agent 侧会话，如果 ACP Server 支持会话删除，则发送 `session/delete` 请求删除 agent 侧会话

### 普通会话状态

普通会话状态以 Server 端会话元数据存储为权威，任何状态变更需立即落盘。

普通会话状态变更
- 新建会话时，会话状态为空闲
- Server 重启后，其上所有普通会话状态应置为空闲
- 当接收 `session/update` ACP 通知的 `state_update` 类型时
  - 若状态为 `running` 或 `requires_action` 则为工作中
  - 若状态为 `idle` 则为空闲

### 普通会话选项

普通会话选项存储在内存中，以 Agent 侧数据为权威
- 新建或恢复 ACP 会话时，存储其会话选项在内存中
- 当发送 `session/set_config_option` ACP 请求时，其响应中的会话选项全量覆盖内存存储
- 当接收 `session/update` ACP 通知的 `config_option_update` 类型时，其通知中的会话选项全量覆盖内存存储

### 普通会话斜杠命令

普通会话斜杠命令存储在内存中，以 Agent 侧数据为权威，当接收 `session/update` ACP 通知的 `available_commands_update` 类型时，其通知中的斜杠命令全量覆盖内存存储。

### 普通会话计划

普通会话计划存储在内存中，以 Agent 侧数据为权威，当接收 `session/update` ACP 通知的 `plan_update` 类型时，其通知中的计划全量覆盖内存存储。

### 普通会话上下文信息

普通会话上下文信息存储在内存中，以 Agent 侧数据为权威，当接收 `session/update` ACP 通知的 `usage_update` 类型时，其通知中的上下文窗口总大小和当前上下文大小全量覆盖内存存储。

### 普通会话删除

当用户请求删除普通会话时，立即从元数据中删除该普通会话，然后发起异步任务清理相关资源（如关闭或删除 agent 侧会话，清理关联的 worktree），随后返回响应。异步清理资源采用尽力而为的方式，不无限重试。

### 普通会话存储

普通会话数据包含三部分
- 元数据：存储在 `~/.amux/session.sqlite` 文件中
  ```SQL
  CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,             -- 会话 ID
    state TEXT NOT NULL,             -- 会话状态
    title TEXT,                      -- 会话标题
    workspace TEXT NOT NULL,         -- 工作目录
    worktree_dir TEXT,               -- worktree 目录
    machine TEXT NOT NULL,           -- 所属机器
    agent TEXT NOT NULL,             -- 所属 Agent
    agent_session_id TEXT,           -- Agent 会话 ID
    created_at INTEGER NOT NULL,     -- 创建时间
    updated_at INTEGER NOT NULL  -- 最近活跃时间
  );
  ```
- 对话历史：存储在 `~/.amux/session.sqlite` 文件中
  - Server 在往 ACP Server 发送 `session/prompt` 成功后，应立即给用户消息赋予消息 ID 并落盘，忽略 ACP Server 的 `session/update` 通知的 `user_message` 和 `user_message_chunk` 类别
  ```SQL
  CREATE TABLE IF NOT EXISTS messages (
      session_id TEXT NOT NULL,      -- Amux 普通会话 ID
      message_id TEXT NOT NULL,      -- 消息 ID：用户消息 ID 由 Amux 生成，Agent 消息 ID 由 ACP Server 提供
      role TEXT NOT NULL,            -- user / agent
      content TEXT NOT NULL,         -- 消息内容，以 json 格式存放
      created_at INTEGER NOT NULL,   -- 创建时间
      updated_at INTEGER NOT NULL,   -- 更新时间
      PRIMARY KEY (session_id, message_id)
  );
  ```
- 活动历史：存储在 `~/.amux/session.sqlite` 文件中
  ```
  CREATE TABLE IF NOT EXISTS activities (
      session_id TEXT NOT NULL,      -- Amux 普通会话 ID
      activity_id TEXT NOT NULL,     -- toolCallId / thought message id / 本地生成的唯一 ID
      kind TEXT NOT NULL,            -- 类别：tool_call / thinking / error
      content TEXT,                  -- 活动内容，以 json 格式存放
      created_at INTEGER NOT NULL,   -- 创建时间
      updated_at INTEGER NOT NULL,   -- 更新时间
      PRIMARY KEY (session_id, activity_id)
  );
  ```

Server 在接收到流式内容后，应按 ACP V2 流式传输的 upsert 语义立即落盘。

### 终端

TODO