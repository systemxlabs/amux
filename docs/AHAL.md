# AHAL — Agent Harness Access Layer

**详略约定**：本文档只描述框架与承重语义（状态机、跨侧保证、职责边界），不描述细节。接口签名以库的代码（TypeScript 类型）为准；数值、边角行为等实现细节交由实现 agent 决定。

AHAL 是 Agent Harness 的控制平面接口规范，以库的形式提供。上层应用（Client）链接 AHAL 库，通过统一接口控制各家的 agent harness（Codex、Claude Code、Kimi Code 等）；库内部由 Driver 组件完成具体 harness 的适配。

---

## 1. 设计原则

| 原则 | 说明 |
|------|------|
| **简洁** | 一个 Driver 接口 + 一个 Session 接口，覆盖 session 创建、prompt 发送、活动控制、事件流的全部需求 |
| **无能力协商** | 接口定义的语义即准入门槛，Driver 必须完整实现，无法满足的 harness 不接入 |
| **无权限往返** | 不做 mid-run 审批，所有 agent 以 yolo 模式运行（自动批准一切操作），安全性完全依赖运行环境 |
| **无 turn 概念** | steer 注入的消息与进行中的工作合并，不存在"请求-响应"边界；底层 harness 的 turn/run 概念是 Driver 内部实现细节 |

非目标：线上协议、跨进程标准、细粒度权限管理、agent 间通信。

---

## 2. 架构

```
┌─────────────────┐
│  上层应用(Client) │
└────────┬────────┘
         │ AHAL 统一接口（本规范）
┌────────┴────────┐
│    AHAL 库       │
│  ┌───────────┐  │   任意方式      ┌──────────────┐
│  │ codex     │──┼── 子进程 ──────►│ codex        │
│  │ driver    │  │   app-server   │ app-server   │
│  ├───────────┤  │               ├──────────────┤
│  │ kimi      │──┼── 进程内 SDK ──►│ kimi-code    │
│  │ driver    │  │               │ SDK          │
│  └───────────┘  │               └──────────────┘
└─────────────────┘
```

- **Client**：链接 AHAL 库的上层应用（控制平面 UI、调度器、自动化脚本）
- **Driver**：适配一种 agent harness 的组件，实现本规范的全部语义。Client 想控制多种 harness，就实例化多个 Driver
- **Harness 适配方式不做规定**：codex driver 可以 spawn `codex app-server` 讲 JSON-RPC，kimi driver 可以进程内调用 kimi-code SDK——只要向上交付本规范定义的语义

---

## 3. 概念模型

AHAL 只有两个核心概念：

- **Session**：与某个 harness 的一段持续会话，有持久化历史，可关闭、可恢复
- **事件流**：Session 上发生的一切，按序投递给 Client

### 3.1 状态模型

Session 有四种状态。`thinking`、`responding`、`acting` 统称"忙"。

| 状态 | 含义 |
|------|------|
| `idle` | 空闲，无进行中的工作 |
| `thinking` | 模型推理中（reasoning / 思考输出阶段） |
| `responding` | 结果输出中（生成对用户可见的回复文本） |
| `acting` | 工具执行中 |

工作区间从 `idle` 进入忙状态开始，到回到 `idle` 结束。忙状态之间可任意转换（包括 `responding → acting`，即模型边输出边调工具），每次迁移都发出事件。

---

## 4. Driver

| 名称 | 语义 |
|---|---|
| `Driver.createSession` | 创建 Session，以 yolo 模式启动 agent|
| `Driver.resumeSession` | 恢复已持久化的 Session（进程重启后）；无法恢复时报错；依赖底层 harness 的持久化 |

---

## 5. Session

| 名称 | 语义 |
|---|---|
| `prompt` | 唯一消息入口：`idle` 启动新工作，忙时 steer |
| `cancel` | 取消进行中的工作，阻塞到完成；Session 已空闲则无操作 |
| `close` | 关闭 Session、释放资源；历史保留可恢复；之后一切调用即报错 |
| `events` | hot stream：事件按序投递，支持多处订阅，订阅时刻起接收后续事件；`close()` 后迭代器结束 |

---

## 6. 事件流

Session 的全部输出以**流式传输**按序投递——消息、思考、工具调用、状态变化都以流式更新表达，每条事件携带产生时间。流式模型参考 ACPv2（Agent Client Protocol）的 `session/update` 设计：全量更新与增量 chunk 并存、按 ID 聚合。

### 6.1 流式语义

- **聚合**：消息与工具更新按 ID 聚合——全量更新可整体替换，chunk 追加；不同消息可交错，Client 按 ID 分别拼接
- **状态**：每次迁移都发；工作区间以 `state_changed`（`idle`）收尾并携带结束原因（正常结束 / 取消 / 达到上限 / 拒绝 / 错误）；`idle` 恒为该区间的最后一条事件
- **顺序与容忍**：事件严格有序；`idle` 后不再出现上一区间的消息/工具事件；Client 容忍不认识的更新类型（未来扩展）

---

## 7. 实现与对接

本节记录各实现包的形态与落地时选用的对接方式（细节以各包代码为准）。

| 包 | 形态 |
|---|---|
| `ahal` | 纯类型与接口（零依赖、无运行时） |
| `ahal-codex` | 采用 `codex app-server` 子进程（JSON-RPC over stdio） |
| `ahal-claude` | 采用 `@anthropic-ai/claude-agent-sdk`（进程内封装） |
| `ahal-kimi` | 采用 `kimi acp`（Agent Client Protocol over stdio） |
