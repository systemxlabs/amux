# 系统设计文档

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
| `agent.list` | 查询当前机器的 agents：agent 名称、agent 可用性 |
| `agent.restart` | 重启指定 agent |
| `agent.skills` | 查询指定 agent 的技能列表 |
| `session.new` | 新建一个普通会话 |
| `session.prompt` | 往指定普通会话发送指令 |
| `session.cancel` | 取消指定普通会话正在进行的工作 |
| `session.delete` | 删除指定普通会话 |
| `session.configure` | 配置指定普通会话：会话标题 |
| `session.history` | 查询指定普通会话的对话历史 |
| `session.activities` | 查询指定普通会话的活动历史 |
| `session.list` | 查询普通会话列表 |

## Server

### 技术栈

- 异步运行时与网络：`tokio`、`tokio-tungstenite`（WebSocket）
- 序列化：`serde` / `serde_json`（JSON-RPC）
- 会话元数据持久化：`rusqlite`
- ACP：`agent-client-protocol` 官方 SDK

### ACP Server 发现

Server 在启动阶段会自动从本机发现当前已安装的 Agent。

| agent | 发现方式 |
|---|---|
| kimi | 本机装有 `kimi` CLI 且 `kimi acp --help` 可用 |
| claude | 本机装有 `claude` CLI 且 npx 可用 |
| codex | 本机装有 `codex` CLI 且 npx 可用 |

### ACP Server 启动

Server 通过子进程方式启动 ACP Server，通过 stdio 来与 ACP Server 通信。

| agent | 启动方式 |
|---|---|
| kimi | `kimi acp` |
| claude | `npx -y @agentclientprotocol/claude-agent-acp` |
| codex | `npx -y @agentclientprotocol/codex-acp` |

### ACP Server 生命周期

Server 启动时会同时启动所有已安装的 ACP Servers，如果 ACP Server 启动失败，则标记不可用。

用户可从应用侧重启某一 ACP Server（无论是否已启动）。

Server 关闭时会同时关闭所有已启动的 ACP Servers，释放相应资源。

### ACP 通信

Server 作为 ACP client 与 ACP servers 通信
- 采用 ACP V1 协议通信
- 权限自动审批

### 普通会话生命周期

### 普通会话存储

## 应用

### 编排智能体

系统提示词应包括
- 角色，工作方式，行为约束
- 工作流执行计划

| 工具 | 用途 |
|---|---|
| `list_agents` | 已注册机器及各机器的 agent 列表：机器在线状态、agent 可用性 |
| `list_sessions` | 本工作流的关联普通会话列表（标题、状态、最近活跃、机器在线与否）|
| `create_session` | 向指定机器、指定 agent 与工作目录创建关联普通会话，返回会话 ID |
| `prompt_session` | 向关联普通会话下发指令 |
| `cancel_session` | 取消关联普通会话进行中的工作 |
| `read_session_history` | 按窗口 / 游标读取关联普通会话对话内容 |
| `read_session_activities` | 按窗口 / 游标读取关联普通会话活动内容 |

### 工作流会话驱动

### 工作流会话存储

### 桌面应用

#### 技术栈

- GUI：`gpui` + `gpui-component`
- 编排智能体：`rig`

### Web 应用
待定

## 可观测性

## 参考
