# 系统设计文档

**详略约定**：本文档只描述框架，不描述实现细节，实现细节交由实现 agent 决定。

## 总体架构

采用三级架构：Daemon 常驻在各个机器上，Server 常驻在公网服务器上，应用运行在客户端。Server 与各机器上的 Daemon 进程通过 WebSocket 通信，应用与 Server 进行 HTTPS 通信。

TODO 架构图

## 通信

### Server-Daemon 通信

Server-Daemon 通信采用 WebSocket，消息格式为 JSON-RPC 2.0。

#### 认证

每个 Daemon 启动时需要指定连接认证 Token，在 Daemon 与 Server 建立 WebSocket 连接握手期间，Daemon 发送的握手请求需携带头部 `Authorization: Bearer <token>`，Server 需要校验 token 是否正确，如不正确则握手失败。

#### 协议

| 方法 | 描述 |
|---|---|
| `agent.discover` | 发现当前机器已安装的 agents |
| `agent.start` | 启动指定 agent |
| `agent.restart` | 重启指定 agent |
| `git.diff` | 查询指定仓库改动 diff |
| `git.restore` | 可按文件或代码块撤销指定仓库的改动 |
| `git.worktree.new` | 从指定仓库创建一个 worktree |
| `git.worktree.resume` | 从指定仓库指定路径恢复 worktree |
| `git.worktree.list` | 查询指定仓库所有 worktrees |
| `git.worktree.delete` | 删除指定 worktree |
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
    "message": "..."
  }
}
```

### 工作树存储

Git worktree 统一存储在 `~/.amux/worktrees/<仓库目录名>-<随机串>/` 内。Git worktree 生命周期由 Server 进行管理。


### 终端存储

Daemon 在内存中存储终端元信息，终端历史由 Server 侧维护。当 Daemon 与 Server 连接断开，其关联的终端资源被释放。
