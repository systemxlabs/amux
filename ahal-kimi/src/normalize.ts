/**
 * kimi ACP 更新 → AHAL 事件的纯归一化层。
 * 无 I/O、可单测。输入 ACP session/update 通知（所用子集）+ 结束原因，输出 AHAL Event。
 *
 * 三态映射（kimi 明确区分思考与回复流）：
 *   - thinking：agent_thought / agent_thought_chunk
 *   - responding：agent_message / agent_message_chunk
 *   - acting：tool_call_update / tool_call_content_chunk
 *
 * 工作区间：首个忙事件开始，以 finish(stopReason)（prompt 完成/state_update idle）收尾。
 * kimi 的 chunk 通知不带 messageId —— 按种类合成稳定 id（agent_message / agent_thought）。
 */
import type {
  ContentBlock,
  Event,
  SessionState,
  StopReason,
  ToolCallStatus,
} from "ahal";

// ---- kimi ACP 更新（所用子集）----

export interface KimiUpdate {
  sessionUpdate: string;
  messageId?: string;
  toolCallId?: string;
  toolName?: string;
  title?: string;
  status?: string;
  state?: "running" | "idle";
  stopReason?: string;
  content?: ContentBlock | ContentBlock[] | { type?: string; [k: string]: unknown };
  [k: string]: unknown;
}

export interface KimiStopReason {
  stopReason?: string;
}

const SYNTH_MESSAGE_ID = "agent_message";
const SYNTH_THOUGHT_ID = "agent_thought";

const BUSY_UPDATE_KINDS = new Set([
  "agent_message",
  "agent_message_chunk",
  "agent_thought",
  "agent_thought_chunk",
  "tool_call_update",
  "tool_call_content_chunk",
  "state_update",
]);

export class KimiNormalizer {
  private state: SessionState = "idle";
  private intervalActive = false;
  private intervalClosed = false;
  private messages = new Map<string, { kind: "agent_message" | "agent_thought"; text: string }>();
  private tools = new Map<string, { toolName: string; status: ToolCallStatus }>();

  snapshot(): { state: SessionState; intervalActive: boolean; intervalClosed: boolean } {
    return { state: this.state, intervalActive: this.intervalActive, intervalClosed: this.intervalClosed };
  }

  push(update: KimiUpdate): Event[] {
    const out: Event[] = [];
    if (this.intervalClosed) {
      // 收尾后：忙碌类更新开启新区间，其余滞留事件过滤
      if (!BUSY_UPDATE_KINDS.has(update.sessionUpdate)) return out;
      this.intervalClosed = false;
    }

    switch (update.sessionUpdate) {
      case "user_message":
        break;
      case "agent_message":
      case "agent_message_chunk": {
        out.push(...this.transition("responding"));
        const mid = update.messageId ?? SYNTH_MESSAGE_ID;
        const content = this.toText(update.content);
        if (update.sessionUpdate === "agent_message") {
          this.messages.set(mid, { kind: "agent_message", text: content });
          out.push({
            kind: "agent_message",
            messageId: mid,
            content: content ? [{ type: "text", text: content }] : [],
          });
        } else {
          const cur = this.messages.get(mid);
          if (!cur) {
            this.messages.set(mid, { kind: "agent_message", text: content });
            out.push({ kind: "agent_message", messageId: mid, content: [] });
          } else {
            cur.text += content;
          }
          out.push({
            kind: "agent_message_chunk",
            messageId: mid,
            content: { type: "text", text: content },
          });
        }
        break;
      }
      case "agent_thought":
      case "agent_thought_chunk": {
        out.push(...this.transition("thinking"));
        const mid = update.messageId ?? SYNTH_THOUGHT_ID;
        const content = this.toText(update.content);
        if (update.sessionUpdate === "agent_thought") {
          this.messages.set(mid, { kind: "agent_thought", text: content });
          out.push({
            kind: "agent_thought",
            messageId: mid,
            content: content ? [{ type: "text", text: content }] : [],
          });
        } else {
          const cur = this.messages.get(mid);
          if (!cur) {
            this.messages.set(mid, { kind: "agent_thought", text: content });
            out.push({ kind: "agent_thought", messageId: mid, content: [] });
          } else {
            cur.text += content;
          }
          out.push({
            kind: "agent_thought_chunk",
            messageId: mid,
            content: { type: "text", text: content },
          });
        }
        break;
      }
      case "tool_call_update": {
        out.push(...this.transition("acting"));
        const toolCallId = update.toolCallId ?? `tool:${this.tools.size + 1}`;
        const tool = this.tools.get(toolCallId) ?? { toolName: update.toolName ?? "tool", status: "in_progress" as ToolCallStatus };
        if (update.toolName) tool.toolName = update.toolName;
        if (update.status) {
          tool.status = update.status as ToolCallStatus;
        }
        this.tools.set(toolCallId, tool);
        const e: { kind: "tool_call_update"; toolCallId: string; toolName: string; title?: string; status: ToolCallStatus } = {
          kind: "tool_call_update",
          toolCallId,
          toolName: tool.toolName,
          status: tool.status,
        };
        if (typeof update.title === "string") e.title = update.title;
        out.push(e);
        break;
      }
      case "tool_call_content_chunk": {
        out.push(...this.transition("acting"));
        const toolCallId = update.toolCallId ?? `tool:${this.tools.size}`;
        const tool = this.tools.get(toolCallId);
        if (!tool) {
          this.tools.set(toolCallId, { toolName: "tool", status: "in_progress" });
          out.push({
            kind: "tool_call_update",
            toolCallId,
            toolName: "tool",
            status: "in_progress",
          });
        }
        const text = this.toText(update.content);
        out.push({
          kind: "tool_call_content_chunk",
          toolCallId,
          content: { type: "text", text },
        });
        break;
      }
      case "state_update": {
        if (update.state === "running") {
          // 具体忙态由具体更新种类驱动；此处确保区间激活
          if (!this.intervalActive) this.intervalActive = true;
        } else if (update.state === "idle") {
          out.push(...this.finish(stopReasonOf(update.stopReason)));
        }
        break;
      }
      default:
        // plan_update / available_commands_update / 未知类型 → 容忍忽略
        break;
    }
    return out;
  }

  /** 外部收尾（prompt 完成 / 取消）：发出唯一的 state_changed(idle, reason) */
  finish(reason?: string): Event[] {
    return this.closeInterval(stopReasonOf(reason));
  }

  private toText(content: unknown): string {
    if (content == null) return "";
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
      return content
        .map((c) => {
          if (typeof c === "string") return c;
          const t = (c as { type?: string; text?: string }).text;
          return typeof t === "string" ? t : "";
        })
        .join("");
    }
    const t = (content as { text?: string }).text;
    return typeof t === "string" ? t : "";
  }

  private transition(next: SessionState): Event[] {
    if (this.state === next) return [];
    this.state = next;
    if (!this.intervalActive) this.intervalActive = true;
    return [{ kind: "state_changed", state: next }];
  }

  private closeInterval(reason: StopReason): Event[] {
    if (!this.intervalActive || this.intervalClosed) return [];
    this.intervalActive = false;
    this.intervalClosed = true;
    this.state = "idle";
    this.messages.clear();
    this.tools.clear();
    return [{ kind: "state_changed", state: "idle", reason }];
  }
}

function stopReasonOf(reason?: string): StopReason {
  switch (reason) {
    case "cancelled":
      return "cancelled";
    case "max_tokens":
      return "max_tokens";
    case "refusal":
      return "refusal";
    case "error":
      return "error";
    default:
      return "end_turn";
  }
}
