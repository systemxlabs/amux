/**
 * 事件 → 视图模型映射（纯函数）：把按序的 ahal 事件折叠为对话流视图
 * （消息气泡、思考气泡、工具调用卡片、状态、用量、错误）。
 * chunk 事件累积到对应气泡/工具的内容；最终事件（agent_message 等）以完整内容为准。
 */

import type { ContentBlock, Event, SessionState, ToolCallStatus } from "ahal";

export interface MessageBubble {
  key: string;
  kind: "message" | "thought" | "user";
  content: ContentBlock[];
  timestamp: number;
  /** 是否已有最终完整内容（区别于仅 chunk 片段） */
  final: boolean;
}

export interface ToolCallView {
  key: string;
  name?: string;
  title?: string;
  status: ToolCallStatus;
  content: ContentBlock[];
}

export interface SessionView {
  bubbles: MessageBubble[];
  tools: ToolCallView[];
  state: SessionState;
  usage?: { context: number; contextWindow: number };
  errors: string[];
  lastEventAt: number;
}

export interface ViewInput {
  event: Event;
  timestamp: number;
}

/** 视图输入项：AHAL 事件或用户消息（server 存储） */
export type ViewItem = ViewInput | { content: ContentBlock[]; timestamp: number };

export function eventsToView(events: readonly ViewItem[]): SessionView {
  const view: SessionView = { bubbles: [], tools: [], state: "idle", errors: [], lastEventAt: 0 };
  const bubbleByKey = new Map<string, MessageBubble>();
  const toolByKey = new Map<string, ToolCallView>();

  for (const item of events) {
    if (item.timestamp > view.lastEventAt) view.lastEventAt = item.timestamp;
    if (!("event" in item)) {
      // 用户消息（server 存储，非 AHAL 事件）
      const key = `user:${item.timestamp}:${view.bubbles.length}`;
      const b: MessageBubble = { key, kind: "user", content: item.content, timestamp: item.timestamp, final: true };
      bubbleByKey.set(key, b);
      view.bubbles.push(b);
      continue;
    }
    const { event, timestamp } = item;
    switch (event.kind) {
      case "agent_message":
      case "agent_thought": {
        const kind = event.kind === "agent_message" ? "message" : "thought";
        const key = `${kind}:${event.messageId}`;
        let b = bubbleByKey.get(key);
        if (!b) {
          b = { key, kind, content: [], timestamp, final: false };
          bubbleByKey.set(key, b);
          view.bubbles.push(b);
        }
        if (event.content) b.content = event.content;
        b.final = true;
        break;
      }
      case "agent_message_chunk":
      case "agent_thought_chunk": {
        const kind = event.kind === "agent_message_chunk" ? "message" : "thought";
        const key = `${kind}:${event.messageId}`;
        let b = bubbleByKey.get(key);
        if (!b) {
          b = { key, kind, content: [], timestamp, final: false };
          bubbleByKey.set(key, b);
          view.bubbles.push(b);
        }
        b.content.push(event.content);
        break;
      }
      case "tool_call_update": {
        let t = toolByKey.get(event.toolCallId);
        if (!t) {
          t = { key: event.toolCallId, status: "pending", content: [] };
          toolByKey.set(event.toolCallId, t);
          view.tools.push(t);
        }
        if (event.toolName !== undefined) t.name = event.toolName;
        if (event.title !== undefined) t.title = event.title;
        if (event.status !== undefined) t.status = event.status;
        if (event.content) t.content = event.content;
        break;
      }
      case "tool_call_content_chunk": {
        let t = toolByKey.get(event.toolCallId);
        if (!t) {
          t = { key: event.toolCallId, status: "pending", content: [] };
          toolByKey.set(event.toolCallId, t);
          view.tools.push(t);
        }
        t.content.push(event.content);
        break;
      }
      case "state_changed":
        view.state = event.state;
        break;
      case "usage_update":
        view.usage = { context: event.context, contextWindow: event.contextWindow };
        break;
      case "error":
        view.errors.push(event.message);
        break;
    }
  }
  return view;
}

/** 内容块 → 纯文本（渲染摘要用；resource 块返回占位描述）。 */
export function contentToText(content: ContentBlock[]): string {
  return content
    .map((c) => {
      if (c.type === "text") return c.text;
      if (c.type === "resource") return `[${c.mimeType} 资源]`;
      return `[引用 ${c.uri}]`;
    })
    .join("\n");
}
