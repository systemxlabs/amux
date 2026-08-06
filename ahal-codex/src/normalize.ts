/**
 * codex app-server v2 协议事件 → AHAL 事件的纯归一化层。
 * 无 I/O、无时间依赖，可单测。输入 codex 协议通知，输出 AHAL Event 列表。
 *
 * 三态映射（如实映射 codex 事件流）：
 *   - thinking：reasoningTextDelta / reasoningSummaryTextDelta / 推理类 item
 *   - responding：agentMessage item 与 agentMessage/delta
 *   - acting：commandExecution / mcpToolCall / dynamicToolCall 等工具类 item
 *
 * 工作区间：从 turn/started（或首个忙事件）开始，以 turn/completed 或线程 idle 收尾，
 * 恒以一个 state_changed(idle, reason) 结束；idle 后过滤上一区间的滞留事件。
 */
import type {
  ContentBlock,
  Event,
  SessionState,
  StopReason,
  ToolCallStatus,
} from "ahal";

// ---- codex v2 协议 wire 事件（所用子集）----

export interface CodexTurn {
  id: string;
  status?: string;
  error?: unknown;
}

export interface CodexItem {
  id: string;
  type: string;
  text?: string;
  command?: string;
  status?: string;
  exitCode?: number | null;
  [k: string]: unknown;
}

export type CodexWireEvent =
  | { method: "turn/started"; params: { threadId: string; turn: CodexTurn } }
  | {
      method: "thread/status/changed";
      params: { threadId: string; status: { type: "idle" | "active"; activeFlags?: string[] } };
    }
  | { method: "item/started"; params: { threadId: string; turnId: string; item: CodexItem } }
  | { method: "item/completed"; params: { threadId: string; turnId: string; item: CodexItem } }
  | {
      method: "item/agentMessage/delta";
      params: { threadId: string; turnId: string; itemId: string; delta: string };
    }
  | {
      method: "reasoningTextDelta" | "reasoningSummaryTextDelta";
      params: { threadId: string; turnId: string; itemId?: string; delta: string };
    }
  | {
      method: "thread/tokenUsage/updated";
      params: {
        threadId: string;
        turnId: string;
        tokenUsage: {
          total: { totalTokens: number; inputTokens: number; outputTokens: number };
          modelContextWindow: number;
        };
      };
    }
  | {
      method: "turn/completed";
      params: { threadId: string; turn: { id: string; status: string; error?: unknown } };
    }
  | { method: "error"; params: { message: string } };

// ---- 归一化器 ----

const TOOL_ITEM_TYPES = new Set(["commandExecution", "mcpToolCall", "dynamicToolCall", "toolCall"]);

function reasonFromTurnStatus(status: string, turn: { error?: unknown }): StopReason {
  switch (status) {
    case "completed":
      return "end_turn";
    case "cancelled":
    case "interrupted":
    case "aborted":
      return "cancelled";
    case "errored":
    case "error":
    case "needsUserInput":
      return "error";
    default:
      return turn.error != null ? "error" : "end_turn";
  }
}

export interface NormalizerState {
  state: SessionState;
  intervalActive: boolean;
  intervalClosed: boolean;
  turnId: string | null;
}

export class CodexNormalizer {
  private state: SessionState = "idle";
  private intervalActive = false;
  private intervalClosed = false;
  private turnId: string | null = null;
  private messages = new Map<string, { kind: "agent_message" | "agent_thought"; text: string }>();
  private tools = new Map<
    string,
    { toolName: string; title: string; status: ToolCallStatus; exitCode: number | null }
  >();
  private seenThoughts = new Set<string>();

  snapshot(): NormalizerState {
    return {
      state: this.state,
      intervalActive: this.intervalActive,
      intervalClosed: this.intervalClosed,
      turnId: this.turnId,
    };
  }

  /** 处理一条 codex 事件，返回要发出的 AHAL 事件（保持顺序） */
  push(ev: CodexWireEvent): Event[] {
    const out: Event[] = [];

    switch (ev.method) {
      case "turn/started": {
        this.turnId = ev.params.turn.id;
        if (!this.intervalActive) {
          this.intervalActive = true;
          this.intervalClosed = false;
          this.messages.clear();
          this.tools.clear();
          this.seenThoughts.clear();
          out.push(...this.transition("thinking"));
        }
        break;
      }

      case "thread/status/changed": {
        // 线程 idle 不立即收尾：turn/completed 随后到达并携带权威状态
        // （codex 通知顺序为 thread idle → turn/completed）
        break;
      }

      case "item/started": {
        if (this.intervalClosed) break;
        const item = ev.params.item;
        if (item.type === "userMessage") break;
        if (item.type === "agentMessage") {
          out.push(...this.transition("responding"));
          const text = item.text ?? "";
          this.messages.set(item.id, { kind: "agent_message", text });
          out.push({
            kind: "agent_message",
            messageId: item.id,
            content: text ? [{ type: "text", text }] : [],
          });
        } else if (TOOL_ITEM_TYPES.has(item.type)) {
          out.push(...this.transition("acting"));
          const toolName = item.type === "commandExecution" ? "command" : item.type;
          const title = typeof item.command === "string" ? item.command : undefined;
          this.tools.set(item.id, {
            toolName,
            title: title ?? "",
            status: "in_progress",
            exitCode: null,
          });
          const update: { kind: "tool_call_update"; toolCallId: string; toolName: string; title?: string; status: ToolCallStatus } = {
            kind: "tool_call_update",
            toolCallId: item.id,
            toolName,
            status: "in_progress",
          };
          if (title) update.title = title;
          out.push(update);
        } else if (item.type === "agentThought" || item.type === "reasoning") {
          out.push(...this.transition("thinking"));
          this.messages.set(item.id, { kind: "agent_thought", text: "" });
          out.push({ kind: "agent_thought", messageId: item.id, content: [] });
        }
        break;
      }

      case "item/completed": {
        if (this.intervalClosed) break;
        const item = ev.params.item;
        if (item.type === "agentMessage") {
          const text = item.text ?? "";
          this.messages.set(item.id, { kind: "agent_message", text });
          out.push({
            kind: "agent_message",
            messageId: item.id,
            content: text ? [{ type: "text", text }] : [],
          });
        } else if (TOOL_ITEM_TYPES.has(item.type)) {
          const tool = this.tools.get(item.id);
          if (tool) {
            const exitCode = typeof item.exitCode === "number" ? item.exitCode : null;
            tool.exitCode = exitCode;
            tool.status = exitCode === 0 || item.status === "completed" ? "completed" : "failed";
            out.push({
              kind: "tool_call_update",
              toolCallId: item.id,
              status: tool.status,
            });
          }
        }
        break;
      }

      case "item/agentMessage/delta": {
        if (this.intervalClosed) break;
        out.push(...this.transition("responding"));
        const cur = this.messages.get(ev.params.itemId);
        if (!cur) {
          this.messages.set(ev.params.itemId, { kind: "agent_message", text: ev.params.delta });
          out.push({ kind: "agent_message", messageId: ev.params.itemId, content: [] });
        } else {
          cur.text += ev.params.delta;
        }
        out.push({
          kind: "agent_message_chunk",
          messageId: ev.params.itemId,
          content: { type: "text", text: ev.params.delta },
        });
        break;
      }

      case "reasoningTextDelta":
      case "reasoningSummaryTextDelta": {
        if (this.intervalClosed) break;
        out.push(...this.transition("thinking"));
        const itemId = ev.params.itemId ?? `thought:${this.turnId ?? "?"}`;
        if (!this.seenThoughts.has(itemId)) {
          this.seenThoughts.add(itemId);
          this.messages.set(itemId, { kind: "agent_thought", text: "" });
          out.push({ kind: "agent_thought", messageId: itemId, content: [] });
        }
        out.push({
          kind: "agent_thought_chunk",
          messageId: itemId,
          content: { type: "text", text: ev.params.delta },
        });
        break;
      }

      case "thread/tokenUsage/updated": {
        const total = ev.params.tokenUsage.total;
        out.push({
          kind: "usage_update",
          context: total.totalTokens,
          contextWindow: ev.params.tokenUsage.modelContextWindow,
        });
        break;
      }

      case "turn/completed": {
        const status = ev.params.turn.status ?? "completed";
        if (status === "errored" || status === "error" || ev.params.turn.error != null) {
          const message =
            typeof ev.params.turn.error === "object" && ev.params.turn.error !== null
              ? JSON.stringify(ev.params.turn.error)
              : String(ev.params.turn.error ?? "turn errored");
          out.push({ kind: "error", message });
        }
        out.push(...this.closeInterval(reasonFromTurnStatus(status, ev.params.turn)));
        break;
      }

      case "error": {
        out.push({ kind: "error", message: ev.params.message });
        break;
      }

      default:
        // 容忍未知通知类型（未来扩展）
        break;
    }

    return out;
  }

  /** 外部收尾（interrupt 超时无 turn/completed）：无条件关闭区间并发出 idle */
  finish(reason: StopReason): Event[] {
    return this.closeInterval(reason);
  }

  /** 状态迁移；每次迁移发一个 state_changed（忙态之间、进入忙态） */
  private transition(next: SessionState): Event[] {
    if (this.state === next) return [];
    this.state = next;
    return [{ kind: "state_changed", state: next }];
  }

  /** 收尾工作区间：发出唯一的 state_changed(idle, reason) */
  private closeInterval(reason: StopReason): Event[] {
    if (!this.intervalActive || this.intervalClosed) return [];
    this.intervalActive = false;
    this.intervalClosed = true;
    this.state = "idle";
    this.turnId = null;
    return [{ kind: "state_changed", state: "idle", reason }];
  }
}
