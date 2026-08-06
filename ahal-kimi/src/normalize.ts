/**
 * @botiverse/kimi-code-sdk 事件 → AHAL 事件的纯归一化层。
 * 无 I/O、可单测。输入 SDK 事件（所用子集），输出 AHAL Event。
 *
 * 三态映射：
 *   - thinking：thinking.delta
 *   - responding：assistant.delta
 *   - acting：tool.call.started / tool.call.delta / tool.result / tool.progress
 *
 * 工作区间：turn.started 开始，turn.ended 收尾（reason 映射结束原因）。
 * SDK 事件无消息级 ID，文本按种类合成稳定 id（agent_message / agent_thought）。
 */
import type { ContentBlock, Event, SessionState, StopReason, ToolCallStatus } from "ahal";

// ---- @botiverse/kimi-code-sdk 事件（所用子集）----

export type KimiSdkEvent =
  | { type: "turn.started"; turnId: number; [k: string]: unknown }
  | { type: "turn.ended"; turnId: number; reason: string; error?: unknown; [k: string]: unknown }
  | { type: "thinking.delta"; turnId: number; delta: string; [k: string]: unknown }
  | { type: "assistant.delta"; turnId: number; delta: string; [k: string]: unknown }
  | {
      type: "tool.call.started";
      turnId: number;
      toolCallId: string;
      name?: string;
      description?: string;
      [k: string]: unknown;
    }
  | { type: "tool.call.delta"; turnId: number; toolCallId?: string; [k: string]: unknown }
  | {
      type: "tool.result";
      turnId: number;
      toolCallId: string;
      output?: unknown;
      isError?: boolean;
      [k: string]: unknown;
    }
  | { type: "tool.progress"; turnId: number; [k: string]: unknown }
  | {
      type: "agent.status.updated";
      contextTokens?: number;
      maxContextTokens?: number;
      [k: string]: unknown;
    }
  | { type: "error"; message?: string; code?: string; [k: string]: unknown };

const SYNTH_MESSAGE_ID = "agent_message";
const SYNTH_THOUGHT_ID = "agent_thought";

const BUSY_TYPES = new Set([
  "turn.started",
  "thinking.delta",
  "assistant.delta",
  "tool.call.started",
  "tool.call.delta",
  "tool.result",
  "tool.progress",
]);

export function isBusySdkEvent(ev: KimiSdkEvent): boolean {
  return BUSY_TYPES.has(ev.type);
}

export class KimiSdkNormalizer {
  private state: SessionState = "idle";
  private intervalActive = false;
  private intervalClosed = false;
  private tools = new Map<string, { toolName: string; status: ToolCallStatus }>();

  snapshot(): { state: SessionState; intervalActive: boolean; intervalClosed: boolean } {
    return { state: this.state, intervalActive: this.intervalActive, intervalClosed: this.intervalClosed };
  }

  push(ev: KimiSdkEvent): Event[] {
    const out: Event[] = [];
    if (this.intervalClosed) {
      // 收尾后：忙碌类事件开启新区间，其余过滤
      if (!isBusySdkEvent(ev)) return out;
      this.intervalClosed = false;
    }

    switch (ev.type) {
      case "turn.started": {
        if (!this.intervalActive) {
          this.intervalActive = true;
          out.push(...this.transition("thinking"));
        }
        break;
      }
      case "thinking.delta": {
        out.push(...this.transition("thinking"));
        out.push({
          kind: "agent_thought_chunk",
          messageId: SYNTH_THOUGHT_ID,
          content: { type: "text", text: ev.delta },
        });
        break;
      }
      case "assistant.delta": {
        out.push(...this.transition("responding"));
        out.push({
          kind: "agent_message_chunk",
          messageId: SYNTH_MESSAGE_ID,
          content: { type: "text", text: ev.delta },
        });
        break;
      }
      case "tool.call.started": {
        out.push(...this.transition("acting"));
        const toolCallId = ev.toolCallId;
        const name = ev.name ?? "tool";
        this.tools.set(toolCallId, { toolName: name, status: "in_progress" });
        const update: { kind: "tool_call_update"; toolCallId: string; toolName: string; title?: string; status: ToolCallStatus } = {
          kind: "tool_call_update",
          toolCallId,
          toolName: name,
          status: "in_progress",
        };
        if (typeof ev.description === "string" && ev.description) update.title = ev.description;
        out.push(update);
        break;
      }
      case "tool.call.delta":
      case "tool.progress": {
        out.push(...this.transition("acting"));
        break;
      }
      case "tool.result": {
        // 工具结束后模型回到思考
        out.push(...this.transition("thinking"));
        const tool = this.tools.get(ev.toolCallId) ?? { toolName: "tool", status: "in_progress" as ToolCallStatus };
        tool.status = ev.isError ? "failed" : "completed";
        this.tools.set(ev.toolCallId, tool);
        out.push({ kind: "tool_call_update", toolCallId: ev.toolCallId, status: tool.status });
        break;
      }
      case "turn.ended": {
        const reason = stopReasonOf(ev.reason);
        if (reason === "error") {
          const detail =
            typeof ev.error === "string"
              ? ev.error
              : ev.error != null
                ? JSON.stringify(ev.error)
                : "kimi turn 失败";
          out.push({ kind: "error", message: detail });
        } else if (ev.reason === "blocked") {
          out.push({ kind: "error", message: "kimi turn 被阻塞" });
        }
        out.push(...this.forceClose(reason));
        break;
      }
      case "agent.status.updated": {
        if (typeof ev.contextTokens === "number") {
          out.push({
            kind: "usage_update",
            context: ev.contextTokens,
            contextWindow: typeof ev.maxContextTokens === "number" ? ev.maxContextTokens : 0,
          });
        }
        break;
      }
      case "error": {
        const message =
          typeof ev.message === "string"
            ? ev.message
            : typeof ev.code === "string"
              ? `kimi 错误: ${ev.code}`
              : "kimi 会话错误";
        out.push({ kind: "error", message });
        break;
      }
      default:
        // 容忍未知事件类型（未来扩展）
        break;
    }
    return out;
  }

  /** 外部收尾（cancel 后无 turn.ended）：无条件关闭区间并发出 idle */
  finish(reason: StopReason): Event[] {
    return this.forceClose(reason);
  }

  private transition(next: SessionState): Event[] {
    if (this.state === next) return [];
    this.state = next;
    if (!this.intervalActive) this.intervalActive = true;
    return [{ kind: "state_changed", state: next }];
  }

  private forceClose(reason: StopReason): Event[] {
    if (this.intervalClosed) return [];
    this.intervalActive = false;
    this.intervalClosed = true;
    this.state = "idle";
    this.tools.clear();
    return [{ kind: "state_changed", state: "idle", reason }];
  }
}

function stopReasonOf(reason: string): StopReason {
  switch (reason) {
    case "cancelled":
      return "cancelled";
    case "failed":
      return "error";
    case "blocked":
      return "refusal";
    default:
      return "end_turn";
  }
}

export type { ContentBlock };
