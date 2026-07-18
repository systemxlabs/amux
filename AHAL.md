# AHAL — Agent Harness Access Layer

版本:0.2(草案)

AHAL 是一个极简的 agent 控制平面**接口规范**,以库的形式提供。上层应用(client)链接 AHAL 库,通过统一接口控制各家的 agent harness(codex、Claude Code、Kimi Code 等);库内部由 driver 组件完成具体 harness 的适配。

AHAL 不规定任何线上通信方式——driver 与 harness 之间如何交互(子进程 + 原生协议、进程内 SDK 等)完全是实现细节。

设计目标:

- **少**:一个 `Driver` 接口 + 一个 `Session` 接口,共 5 个方法 + 1 个事件流,覆盖 session 创建、prompt 发送、活动控制、事件流的全部需求
- **无能力协商**:接口定义的语义即准入门槛,driver 必须完整实现,不支持 steer 的 harness 不接入
- **无权限往返**:不做 mid-run 审批,无任何安全策略——所有 agent 以 yolo 模式运行(自动批准一切操作),安全性完全依赖运行环境(容器、专用机器等),不在接口范围内
- **无 plan 模式**:plan 输出降级为普通事件透出,不建模状态

非目标:线上协议、跨进程标准、细粒度权限管理、agent 间通信。

---

## 1. 架构

```
┌─────────────────┐
│  上层应用(client) │
└────────┬────────┘
         │ AHAL 统一接口(本规范)
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

- **client**:链接 AHAL 库的上层应用(控制平面 UI、调度器、自动化脚本)
- **driver**:适配一种 agent harness 的组件,实现本规范的全部接口与语义。client 想控制多种 harness,就实例化多个 driver
- **harness 适配方式不做规定**:codex driver 可以 spawn `codex app-server` 讲 JSON-RPC,kimi driver 可以进程内调用 kimi-code SDK,也可以是任何其他方式——只要向上交付本规范定义的语义

## 2. 概念模型

AHAL 只有两个概念:

- **session**:与某个 harness 的一段持续会话,有持久化历史,可关闭、可恢复
- **事件流**:session 上发生的一切,按序投递给 client

session 内部状态只有 `idle | busy`:`prompt()` 在 idle 时启动工作、在 busy 时注入(steer)进行中的工作;agent 从 busy 变 idle 时发出 `idle` 事件。

**没有 turn 概念。** steer 注入的消息与进行中的工作合并,不存在"请求-响应"的边界;底层 harness 的 turn/run 概念(如 codex 的 turnId)是 driver 的内部实现细节,不向上暴露。

## 3. 接口定义

以下用 TypeScript 类型记号作为规范记法;其他语言的实现 MUST 提供一一对应的结构。

### 3.1 Driver

```typescript
interface Driver {
  readonly runtime: string;        // harness 标识,如 "codex" | "kimi" | "claude"
  readonly ahalVersion: number;    // 实现的 AHAL 规范版本

  createSession(options: SessionOptions): Promise<Session>;
  resumeSession(sessionId: SessionId): Promise<Session>;
}

interface SessionOptions {
  cwd: string;              // 工作目录,必选
  model?: string;           // 模型标识,缺省用 harness 默认
}
```

- `createSession`:创建 session。driver MUST 以 yolo 模式启动 agent(如 codex `--dangerously-bypass-approvals-and-sandbox`、kimi `--yolo`),并自动批准/屏蔽底层 harness 的一切审批请求,不向 client 透出
- `resumeSession`:恢复一个持久化的 session(进程重启后)。driver MUST 持久化 session 元数据与对话历史(依赖底层 harness 的持久化,如 `~/.codex/sessions`);无法恢复时 reject `SessionNotFoundError`

### 3.2 Session

```typescript
interface Session {
  readonly id: SessionId;
  readonly cwd: string;

  prompt(input: Input): Promise<void>;
  cancel(): Promise<boolean>;

  readonly events: AsyncIterable<SessionEvent>;  // 唯一的事件流

  close(): Promise<void>;
}
```

#### prompt()

**唯一的消息发送入口**,自适应语义,resolve 即送达:

- session idle → 启动新工作
- session busy → 作为 steer 注入进行中的工作

```typescript
type Input = ContentBlock[];

type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; path: string };  // harness 不支持图片时 reject InvalidInputError
```

- **不区分"启动了新工作"还是"注入了进行中的工作"**——对 client 而言只有"送达"与"未送达"(reject)。工作边界的归属是 driver 内部事务
- **原子性**:消息必然送达 agent(注入当前工作或作为新输入),不存在丢失或第三种去向。工作恰好在调用处理期间结束时,driver MUST 将消息作为新输入启动工作
- **有序性**:同一 session 上连续调用的多条消息,按调用顺序送达 agent
- **follow-up 语义**:接口不提供队列。client 若需要"等当前工作做完再做下一件",应自行等待 `idle` 事件后再调用(见 §5.3)

#### cancel()

取消当前进行中的工作,返回是否确实有工作被取消(空闲时返回 `false`,不算错误)。

- 底层 harness 无协议层 interrupt 能力时,driver MUST kill 底层进程并以原 session 上下文重建,对 client 保持语义一致
- cancel 后事件流 MUST 继续投递尾部事件,直到发出 `idle` 事件(`reason: "cancelled"`)

#### events

session 的全部事件流,按序投递。

```typescript
interface SessionEvent {
  event: Event;
}

type Event =
  | { kind: "text"; text: string }                    // 增量,client 自行拼接
  | { kind: "thinking"; text: string }                // 增量;harness 不输出思考时 driver 不发
  | { kind: "tool_call"; toolCallId: string; name: string;
      status: "started" | "output" | "completed" | "failed";
      input?: unknown; output?: string }              // output 状态携带增量输出
  | { kind: "subagent"; subagentId: string; status: string; description?: string }
  | { kind: "compaction_started" }
  | { kind: "compaction_finished" }
  | { kind: "usage"; inputTokens: number; outputTokens: number; cost?: number }
  | { kind: "error"; message: string; fatal: boolean }
  | { kind: "idle"; reason: IdleReason; usage?: Usage }
  | { kind: "background_task"; taskId: string;
      status: "completed" | "failed"; description?: string; result?: string }
  | { kind: "notice"; message: string };

type IdleReason =
  | "completed"   // 工作正常完成
  | "cancelled"   // 被 cancel() 取消
  | "error";      // 异常终止,driver SHOULD 在其前发送 error 事件说明原因

interface Usage {
  inputTokens: number;
  outputTokens: number;
}
```

`idle` 事件是关键事件:agent 从 busy 变 idle 时发出,标志一段工作区间的结束。driver MUST 保证:

- 每段工作区间恰好以一个 `idle` 事件收尾,即使工作因错误、cancel 或 driver 内部异常终止
- `idle` 恒为该区间的最后一个事件(可在投递前 drain 底层通道的滞留事件)

其余事件规则:

- `text` / `thinking` 为**增量**,client 自行拼接
- busy 期间的事件严格有序;`background_task` / `notice` 这类主动事件可在任意时刻出现(包括 idle 期间),client 不得假设它们落在某个工作区间内
- client MUST 容忍不认识的 `kind`(未来扩展),不得中断事件流消费

#### close()

关闭 session,释放底层资源(子进程、连接等)。session 的对话历史仍被持久化,之后可通过 `resumeSession` 恢复。

### 3.3 错误类型

```typescript
class AhalError extends Error {}
class SessionNotFoundError extends AhalError {}      // resumeSession 的 session 不存在或无法恢复
class HarnessUnavailableError extends AhalError {}   // 底层 harness 不可用(未安装、版本不兼容)
class SessionBusyError extends AhalError {}          // session 正在重建中,暂时不可写
class InvalidInputError extends AhalError {}         // 输入非法或过大
```

其他语言的实现 MUST 提供可区分的等价错误类型,不得用裸字符串表达错误类别。

## 4. 生命周期

```
driver = createDriver("codex")
  └─► driver.createSession({ cwd }) ──► session(idle)
        └─► session.prompt(input) ──► busy ──► events* ──► idle 事件
              │                              ▲
              └──── session.prompt(steer 注入)┘
        └─► session.cancel() ──► idle 事件(reason=cancelled)
        └─► session.close()
  └─► driver.resumeSession(id) ──► ...(继续使用)
```

## 5. 语义细则

### 5.1 cancel 竞态

`cancel()` 与工作自然结束存在竞态:cancel 调用时工作可能刚完成。规定:

- 以 `idle` 事件为唯一事实来源:无论谁先谁后,client 只根据收到的 `idle.reason` 判断结局

### 5.2 瞬态窗口与尾部事件

- 底层 harness 可能存在短暂拒收 steer 的窗口(如 tool call 刚结束时)。driver MUST 内部缓冲并在窗口关闭后重试,窗口期 MUST 有上限(建议 ≤ 5s)。这一切对 client 不可见——`prompt()` 的 resolve 表示"driver 已受理并保证送达",不代表"此刻已注入"
- 超时仍无法注入时,driver MUST 保证消息不丢:作为新输入启动工作,并发送 `error` 事件(`fatal: false`)说明发生了降级
- cancel 或工作自然结束后,底层通道上仍可能有滞留事件。driver MUST 过滤这些 stragglers,保证 `idle` 之后不再出现属于上一区间的 `text`/`tool_call` 等事件;client 无需做任何迟到检测

### 5.3 follow-up 模式(客户端约定)

接口无队列。需要"做完 A 再做 B"的 client:

```
session.prompt(A) → 等 idle 事件 → session.prompt(B)
```

`idle` 之后 session 必然空闲,此时 B 必然作为新工作启动。

### 5.4 client 崩溃与恢复

- session 不随 client 使用方释放而销毁:只要底层 harness 的持久化还在,client 重启后可 `resumeSession` 继续
- driver 实例本身崩溃时,进行中的工作结局未知;client MUST 在 `resumeSession` 后容忍"上一段工作没有收到 `idle` 事件"的情况,直接开始新工作

## 6. 完整示例(TypeScript)

```typescript
import { createDriver } from "ahal";

const driver = createDriver("kimi");
const session = await driver.createSession({ cwd: "/srv/app" });

// 后台消费事件流
(async () => {
  for await (const { event } of session.events) {
    switch (event.kind) {
      case "text":
        process.stdout.write(event.text);
        break;
      case "idle":
        console.log(`\n工作结束: ${event.reason}`, event.usage);
        break;
      case "background_task":
        console.log(`后台任务 ${event.taskId}: ${event.status}`);
        break;
    }
  }
})();

// 发起任务——resolve 即送达
await session.prompt([{ type: "text", text: "修复 login 的测试失败" }]);

// 进行中插话 → 自动成为 steer,接口上无差别
await session.prompt([{ type: "text", text: "别改 fixture,问题在源码" }]);

// follow-up:等 idle 事件后再发
```
