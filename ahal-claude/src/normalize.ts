/**
 * claude-agent-sdk 消息流 → AHAL 事件的纯归一化层。
 * 无 I/O、可单测。输入 SDK 消息（所用子集），输出 AHAL Event。
 *
 * 三态映射：
 *   - thinking：content_block_start/thinking_delta（thinking 块）、thinking_tokens
 *   - responding：content_block_start/text_delta（文本块）
 *   - acting：content_block_start（tool_use 块）、tool_progress、tool_result
 *
 * 工作区间：首个忙事件开始，以 result 消息收尾 → state_changed(idle, reason)。
 */
import type {
  ContentBlock,
  Event,
  SessionState,
  StopReason,
  ToolCallStatus,
} from "ahal";

// ---- claude-agent-sdk 消息（所用子集）----

export interface ClaudeRawStreamEvent {
  type: "message_start" | "message_delta" | "message_stop" | "content_block_start" | "content_block_delta" | "content_block_stop" | "ping" | "error";
  message?: { id: string };
  index?: number;
  content_block?: {
    type: string;
    id?: string;
    name?: string;
    text?: string;
  };
  delta?:
    | { type: "text_delta"; text: string }
    | { type: "thinking_delta"; thinking: string }
    | { type: "signature_delta"; signature: string }
    | { type: "input_json_delta"; partial_json: string }
    | { type: "stop_reason" | "stop_sequence"; [k: string]: unknown };
  stop_reason?: string | null;
  error?: { type?: string; message?: string };
}

export type ClaudeMessage =
  | {
      type: "stream_event";
      event: ClaudeRawStreamEvent;
      session_id?: string;
      uuid?: string;
    }
  | {
      type: "assistant";
      message: {
        id: string;
        content?: unknown;
      };
      session_id?: string;
      uuid?: string;
    }
  | {
      type: "user";
      message: { content?: unknown };
      session_id?: string;
      uuid?: string;
    }
  | {
      type: "system";
      subtype?: string;
      session_id?: string;
      uuid?: string;
    }
  | {
      type: "result";
      subtype: "success" | "error";
      session_id?: string;
      is_error?: boolean;
      result?: string;
      stop_reason?: string | null;
      error?: unknown;
      [k: string]: unknown;
    };

export interface ToolResultBlock {
  type: "tool_result";
  tool_use_id: string;
  is_error?: boolean;
  content?: unknown;
}

/** 忙碌类消息：可开启新区间 */
export function isBusyMessage(msg: ClaudeMessage): boolean {
  return msg.type === "stream_event" || msg.type === "assistant" || msg.type === "user";
}

export interface UserMessageParam {
  role: "user";
  content: ToolResultBlock[] | string;
}

// ---- 归一化器 ----

export class ClaudeNormalizer {
  private state: SessionState = "idle";
  private intervalActive = false;
  private intervalClosed = false;
  private msgId = "";
  private blockIndexToTool = new Map<number, string>();
  private messages = new Map<string, { kind: "agent_message" | "agent_thought"; text: string }>();
  private tools = new Map<string, { toolName: string; status: ToolCallStatus }>();

  snapshot(): { state: SessionState; intervalActive: boolean; intervalClosed: boolean } {
    return { state: this.state, intervalActive: this.intervalActive, intervalClosed: this.intervalClosed };
  }

  /** 外部收尾（abort/cancel 后可能无 result 也无忙事件）：无条件关闭区间并发出 idle */
  finish(reason: StopReason): Event[] {
    return this.forceClose(reason);
  }

  /** 无条件关闭区间（result / 外部收尾均视为 turn 结束） */
  private forceClose(reason: StopReason): Event[] {
    if (this.intervalClosed) return [];
    this.intervalActive = false;
    this.intervalClosed = true;
    this.state = "idle";
    this.messages.clear();
    this.tools.clear();
    return [{ kind: "state_changed", state: "idle", reason }];
  }

  push(msg: ClaudeMessage): Event[] {
    const out: Event[] = [];
    if (this.intervalClosed) {
      // 收尾后：忙碌类消息开启新区间，其余滞留消息过滤（result 不再开区间）
      if (msg.type === "result" || !isBusyMessage(msg)) return out;
      this.intervalClosed = false;
    }

    if (msg.type === "stream_event") {
      out.push(...this.handleStreamEvent(msg.event));
    } else if (msg.type === "assistant") {
      out.push(...this.handleAssistant(msg.message));
    } else if (msg.type === "user") {
      out.push(...this.handleUserMessage(msg.message));
    } else if (msg.type === "system") {
      if (msg.subtype === "thinking_tokens") {
        out.push(...this.transition("thinking"));
      }
    } else if (msg.type === "result") {
      if (msg.subtype === "error" || msg.is_error) {
        const detail =
          typeof msg.error === "string"
            ? msg.error
            : msg.error != null
              ? JSON.stringify(msg.error)
              : `claude turn errored (${msg.subtype ?? "?"})`;
        out.push({ kind: "error", message: detail });
        out.push(...this.forceClose("error"));
      } else {
        out.push(...this.forceClose(stopReason(msg.stop_reason)));
      }
    }
    return out;
  }

  private handleStreamEvent(ev: ClaudeRawStreamEvent): Event[] {
    const out: Event[] = [];
    switch (ev.type) {
      case "message_start":
        this.msgId = ev.message?.id ?? this.msgId;
        break;
      case "content_block_start": {
        const block = ev.content_block;
        if (!block || ev.index === undefined) break;
        if (block.type === "text") {
          out.push(...this.transition("responding"));
          const mid = `${this.msgId}#${ev.index}`;
          const text = block.text ?? "";
          this.messages.set(mid, { kind: "agent_message", text });
          out.push({
            kind: "agent_message",
            messageId: mid,
            content: text ? [{ type: "text", text }] : [],
          });
        } else if (block.type === "thinking" || block.type === "redacted_thinking") {
          out.push(...this.transition("thinking"));
          const mid = `${this.msgId}#${ev.index}`;
          this.messages.set(mid, { kind: "agent_thought", text: "" });
          out.push({ kind: "agent_thought", messageId: mid, content: [] });
        } else if (block.type === "tool_use") {
          out.push(...this.transition("acting"));
          const toolCallId = block.id ?? `${this.msgId}#${ev.index}`;
          this.blockIndexToTool.set(ev.index, toolCallId);
          this.tools.set(toolCallId, { toolName: block.name ?? "tool", status: "in_progress" });
          out.push({
            kind: "tool_call_update",
            toolCallId,
            toolName: block.name ?? "tool",
            status: "in_progress",
          });
        }
        break;
      }
      case "content_block_delta": {
        const delta = ev.delta;
        if (!delta || ev.index === undefined) break;
        if (delta.type === "text_delta") {
          out.push(...this.transition("responding"));
          const mid = `${this.msgId}#${ev.index}`;
          const cur = this.messages.get(mid);
          if (!cur) {
            this.messages.set(mid, { kind: "agent_message", text: delta.text });
            out.push({ kind: "agent_message", messageId: mid, content: [] });
          } else {
            cur.text += delta.text;
          }
          out.push({
            kind: "agent_message_chunk",
            messageId: mid,
            content: { type: "text", text: delta.text },
          });
        } else if (delta.type === "thinking_delta") {
          out.push(...this.transition("thinking"));
          const mid = `${this.msgId}#${ev.index}`;
          const cur = this.messages.get(mid);
          if (!cur) {
            this.messages.set(mid, { kind: "agent_thought", text: "" });
            out.push({ kind: "agent_thought", messageId: mid, content: [] });
          } else {
            cur.text += delta.thinking;
          }
          out.push({
            kind: "agent_thought_chunk",
            messageId: mid,
            content: { type: "text", text: delta.thinking },
          });
        } else if (delta.type === "input_json_delta") {
          out.push(...this.transition("acting"));
          const toolCallId = this.blockIndexToTool.get(ev.index);
          if (toolCallId) {
            out.push({
              kind: "tool_call_content_chunk",
              toolCallId,
              content: { type: "text", text: delta.partial_json },
            });
          }
        }
        break;
      }
      case "content_block_stop": {
        if (ev.index !== undefined) {
          const toolCallId = this.blockIndexToTool.get(ev.index);
          if (toolCallId) {
            this.blockIndexToTool.delete(ev.index);
            // 工具块结束不立即终态；等 tool_result（见 user 消息）置 completed/failed
          }
        }
        break;
      }
      case "message_delta":
        // stop_reason 由 result 消息携带，此处忽略
        break;
      case "error": {
        const message = ev.error?.message ?? "claude stream error";
        out.push({ kind: "error", message });
        break;
      }
      default:
        break;
    }
    return out;
  }

  /** 整条 assistant 消息（SDK 默认路径：无 stream_event 时一次送达完整 content 块） */
  private handleAssistant(message: { id: string; content?: unknown }): Event[] {
    const out: Event[] = [];
    const content = message.content;
    if (!Array.isArray(content)) return out;
    content.forEach((block, index) => {
      const b = block as {
        type?: string;
        text?: string;
        thinking?: string;
        id?: string;
        name?: string;
      };
      if (!b || typeof b.type !== "string") return;
      if (b.type === "text") {
        out.push(...this.transition("responding"));
        const mid = `${message.id}#${index}`;
        const text = b.text ?? "";
        this.messages.set(mid, { kind: "agent_message", text });
        out.push({
          kind: "agent_message",
          messageId: mid,
          content: text ? [{ type: "text", text }] : [],
        });
      } else if (b.type === "thinking" || b.type === "redacted_thinking") {
        out.push(...this.transition("thinking"));
        const mid = `${message.id}#${index}`;
        const text = b.thinking ?? "";
        this.messages.set(mid, { kind: "agent_thought", text });
        out.push({
          kind: "agent_thought",
          messageId: mid,
          content: text ? [{ type: "text", text }] : [],
        });
      } else if (b.type === "tool_use") {
        out.push(...this.transition("acting"));
        const toolCallId = b.id ?? `${message.id}#${index}`;
        this.tools.set(toolCallId, { toolName: b.name ?? "tool", status: "in_progress" });
        out.push({
          kind: "tool_call_update",
          toolCallId,
          toolName: b.name ?? "tool",
          status: "in_progress",
        });
      }
    });
    return out;
  }

  private handleUserMessage(message: unknown): Event[] {
    const out: Event[] = [];
    const content = (message as { content?: unknown })?.content;
    if (Array.isArray(content)) {
      for (const block of content as unknown[]) {
        const b = block as ToolResultBlock;
        if (b?.type === "tool_result" && typeof b.tool_use_id === "string") {
          out.push(...this.transition("thinking"));
          const tool = this.tools.get(b.tool_use_id);
          if (tool) {
            tool.status = b.is_error ? "failed" : "completed";
            out.push({
              kind: "tool_call_update",
              toolCallId: b.tool_use_id,
              status: b.is_error ? "failed" : "completed",
            });
          }
        }
      }
    }
    return out;
  }

  private transition(next: SessionState): Event[] {
    if (this.state === next) return [];
    this.state = next;
    if (!this.intervalActive) this.intervalActive = true;
    return [{ kind: "state_changed", state: next }];
  }

}

function stopReason(reason: string | null | undefined): StopReason {
  switch (reason) {
    case "end_turn":
      return "end_turn";
    case "max_tokens":
      return "max_tokens";
    case "refusal":
      return "refusal";
    case "stop_sequence":
      return "end_turn";
    default:
      return "end_turn";
  }
}

export type { ContentBlock };
