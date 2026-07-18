# AHAL — Agent Harness Access Layer

版本:0.1(草案)

AHAL 是一个极简的 agent 控制平面**接口规范**,以库的形式提供。上层应用(client)链接 AHAL 库,通过统一接口控制各家的 agent harness(codex、Claude Code、Kimi Code 等);库内部由 driver 组件完成具体 harness 的适配。

AHAL 不规定任何线上通信方式——driver 与 harness 之间如何交互(子进程 + 原生协议、进程内 SDK 等)完全是实现细节。

设计目标:

- **少**:一个 `Driver` 接口 + 一个 `Session` 接口,共 5 个方法 + 1 个事件流,覆盖 session 创建、prompt 发送、turn 控制、事件流的全部需求
- **无能力协商**:接口定义的语义即准入门槛,driver 必须完整实现,不支持 steer 的 harness 不接入
- **无权限往返**:不做 mid-turn 审批,无任何安全策略——所有 agent 以 yolo 模式运行(自动批准一切操作),安全性完全依赖运行环境(容器、专用机器等),不在接口范围内
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

## 2. 接口定义

以下用 TypeScript 类型记号作为规范记法;其他语言的实现 MUST 提供一一对应的结构。

### 2.1 Driver

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

### 2.2 Session

```typescript
interface Session {
  readonly id: SessionId;
  readonly cwd: string;

  prompt(input: Input): Promise<PromptResult>;
  cancel(): Promise<boolean>;

  readonly events: AsyncIterable<SessionEvent>;  // 唯一的事件流

  close(): Promise<void>;
}
```

#### prompt()

**唯一的消息发送入口**,自适应语义:

- session 空闲(无进行中的 turn)→ 开启新 turn
- session 有进行中的 turn → 作为 steer 注入当前 turn

```typescript
type Input = ContentBlock[];

type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; path: string };  // harness 不支持图片时 reject InvalidInputError

interface PromptResult {
  acceptedAs: "prompted" | "steered" | "busy";
  turnId: TurnId;
}
```

`acceptedAs`:

| 值 | 说明 |
|---|---|
| `"prompted"` | 开启了一个新 turn |
| `"steered"` | 注入了进行中的 turn |
| `"busy"` | steer 因底层瞬态窗口(见 §4.2)未能立即送达,driver 将尽快重试注入,**不丢消息** |

语义保证:

1. **原子性**:一条消息必然落入"当前 turn"或"新 turn"之一,不存在丢失或第三种去向。turn 恰好在调用处理期间结束时,driver MUST 将消息降级为新 turn 的 prompt(`acceptedAs: "prompted"`)
2. **有序性**:同一 session 上连续调用的多条消息,按调用顺序生效
3. **follow-up 语义**:接口不提供队列。client 若需要"等当前任务做完再做下一件",应自行等待 `turn_end` 事件后再调用(见 §4.3)

#### cancel()

取消当前 turn,返回是否确实有 turn 被取消(无进行中的 turn 时返回 `false`,不算错误)。

- 底层 harness 无协议层 interrupt 能力时,driver MUST kill 底层进程并以原 session 上下文重建,对 client 保持语义一致
- cancel 后事件流 MUST 继续投递尾部事件,直到发出 `turn_end` 事件(`stopReason: "cancelled"`)

#### events

session 的全部事件流,承载两类事件:

- **turn 内事件**:turn 的全部事件,包括 turn 的结束
- **turn 外事件**:driver/agent 主动发起的事件(后台任务完成、定时提醒等),client 不可请求,只能接收

```typescript
interface SessionEvent {
  turnId?: TurnId;   // 仅 turn 内事件携带;turn 外事件省略
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
  | { kind: "turn_end"; stopReason: StopReason; usage?: Usage }
  // ── 以下为 turn 外事件(无 turnId)──
  | { kind: "background_task"; taskId: string;
      status: "completed" | "failed"; description?: string; result?: string }
  | { kind: "notice"; message: string };

type StopReason =
  | "end_turn"    // 正常完成
  | "cancelled"   // 被 cancel() 取消
  | "error";      // 异常终止,driver SHOULD 在其前发送 error 事件说明原因

interface Usage {
  inputTokens: number;
  outputTokens: number;
}
```

事件规则:

- 同一 turn 内事件严格有序,`turn_end` 恒为该 turn 最后一个事件
- driver MUST 保证每个 turn 恰好发出一个 `turn_end`,即使 turn 因错误、cancel 或 driver 内部异常终止
- turn 外事件与任何 turn 内事件无相对顺序保证,client MUST 按无 `turnId` 识别并独立处理,不得混入当前 turn 的事件流
- client MUST 容忍不认识的 `kind`(未来扩展),不得中断事件流消费

#### close()

关闭 session,释放底层资源(子进程、连接等)。session 的对话历史仍被持久化,之后可通过 `resumeSession` 恢复。

### 2.3 错误类型

```typescript
class AhalError extends Error {}
class SessionNotFoundError extends AhalError {}      // resumeSession 的 session 不存在或无法恢复
class HarnessUnavailableError extends AhalError {}   // 底层 harness 不可用(未安装、版本不兼容)
class SessionBusyError extends AhalError {}          // session 正在重建中,暂时不可写
class InvalidInputError extends AhalError {}         // 输入非法或过大
```

其他语言的实现 MUST 提供可区分的等价错误类型,不得用裸字符串表达错误类别。

## 3. 生命周期

```
driver = createDriver("codex")
  └─► driver.createSession({ cwd }) ──► session
        └─► session.prompt(input) ──► turn 开始 ──► events* ──► turn_end 事件
              │                                     ▲
              └──── session.prompt(steer 注入)───────┘
        └─► session.cancel() ──► turn_end 事件(stopReason=cancelled)
        └─► session.close()
  └─► driver.resumeSession(id) ──► ...(继续使用)
```

turn 状态机极简:`idle → running → idle`。turn 只由 `prompt()`(空闲时)开始,由事件流中的 `turn_end` 事件标志结束。

## 4. 语义细则

### 4.1 cancel 竞态

`cancel()` 与 turn 自然结束存在竞态:cancel 调用时 turn 可能刚完成。规定:

- 以 `turn_end` 事件为唯一事实来源:无论谁先谁后,client 只根据收到的 `turn_end.stopReason` 判断结局

### 4.2 瞬态窗口与尾部事件

- 底层 harness 可能存在短暂拒收 steer 的窗口(如 tool call 刚结束时)。此时 `prompt()` 返回 `acceptedAs: "busy"`,driver 内部缓冲并尽快重试,窗口期 MUST 有上限(建议 ≤ 5s),超时仍未注入则降级为新 turn 并发送 `error` 事件说明
- cancel 或 turn 自然结束后、底层通道上仍可能有滞留事件。driver MUST 保证 `turn_end` 是该 turn 最后一个投递的事件(可在投递 `turn_end` 前 drain 尾部事件);client MUST 按 `turnId` 丢弃迟到于 `turn_end` 的事件

### 4.3 follow-up 模式(客户端约定)

接口无队列。需要"做完 A 再做 B"的 client:

```
session.prompt(A) → 等 turn_end 事件 → session.prompt(B)
```

`turn_end` 后 session 必然空闲,此时 B 必然 `acceptedAs: "prompted"`。

### 4.4 client 崩溃与恢复

- session 不随 client 使用方释放而销毁:只要底层 harness 的持久化还在,client 重启后可 `resumeSession` 继续
- driver 进程/实例本身崩溃时,进行中的 turn 结局未知;client MUST 在 `resumeSession` 后容忍"上一个 turn 没有收到 `turn_end`"的情况,直接开始新 turn

## 5. 完整示例(TypeScript)

```typescript
import { createDriver } from "ahal";

const driver = createDriver("kimi");
const session = await driver.createSession({ cwd: "/srv/app" });

// 后台消费事件流
(async () => {
  for await (const { turnId, event } of session.events) {
    switch (event.kind) {
      case "text":
        process.stdout.write(event.text);
        break;
      case "turn_end":
        console.log(`\nturn ${turnId} 结束: ${event.stopReason}`, event.usage);
        break;
      case "background_task":
        console.log(`后台任务 ${event.taskId}: ${event.status}`);
        break;
    }
  }
})();

// 发起任务
const r1 = await session.prompt([{ type: "text", text: "修复 login 的测试失败" }]);
console.log(r1.acceptedAs); // "prompted"

// turn 进行中插话 → 自适应为 steer
const r2 = await session.prompt([{ type: "text", text: "别改 fixture,问题在源码" }]);
console.log(r2.acceptedAs); // "steered"

// follow-up:等 turn_end 后再发,必然 "prompted"
```
