import { describe, expect, it } from "vitest";
import {
  AhalError,
  HarnessUnavailableError,
  InvalidInputError,
  PromptTimeoutError,
  SessionBusyError,
  SessionClosedError,
  SessionNotFoundError,
} from "../src/index.js";
import type { Event, SessionState } from "../src/index.js";

describe("ahal 错误类型", () => {
  it("全部错误继承 AhalError 且名称可区分", () => {
    const errors = [
      new AhalError(),
      new SessionNotFoundError("x"),
      new HarnessUnavailableError("x"),
      new SessionBusyError("x"),
      new SessionClosedError("x"),
      new PromptTimeoutError("x"),
      new InvalidInputError("x"),
    ];
    for (const e of errors) {
      expect(e).toBeInstanceOf(Error);
      expect(e).toBeInstanceOf(AhalError);
    }
    const names = errors.map((e) => e.name);
    expect(new Set(names).size).toBe(names.length); // 名称两两不同
    expect(new SessionClosedError("c").message).toBe("c");
  });
});

describe("ahal 类型面（编译期约束）", () => {
  it("SessionState 四态可赋值", () => {
    const states: SessionState[] = ["idle", "thinking", "responding", "acting"];
    expect(states).toHaveLength(4);
  });

  it("事件联合类型覆盖全部种类", () => {
    // 编译期：以下字面量必须匹配 Event 联合
    const events: Event[] = [
      { kind: "agent_message", messageId: "m1" },
      { kind: "agent_message_chunk", messageId: "m1", content: { type: "text", text: "a" } },
      { kind: "agent_thought", messageId: "t1" },
      { kind: "agent_thought_chunk", messageId: "t1", content: { type: "text", text: "b" } },
      { kind: "tool_call_update", toolCallId: "c1", status: "in_progress" },
      { kind: "tool_call_content_chunk", toolCallId: "c1", content: { type: "text", text: "o" } },
      { kind: "state_changed", state: "idle", reason: "end_turn" },
      { kind: "usage_update", context: 1, contextWindow: 200000 },
      { kind: "error", message: "boom" },
    ];
    expect(events).toHaveLength(9);
    expect(events.map((e) => e.kind)).toEqual([
      "agent_message",
      "agent_message_chunk",
      "agent_thought",
      "agent_thought_chunk",
      "tool_call_update",
      "tool_call_content_chunk",
      "state_changed",
      "usage_update",
      "error",
    ]);
  });

  it("内容块三形态（text / resource 二选一 / resource_link）", () => {
    const blocks: Event[] = [
      { kind: "agent_message", messageId: "x", content: [{ type: "text", text: "t" }] },
      {
        kind: "agent_message",
        messageId: "x",
        content: [{ type: "resource", mimeType: "image/png", blob: "abc" }],
      },
      {
        kind: "agent_message",
        messageId: "x",
        content: [{ type: "resource_link", uri: "file:///a", name: "a" }],
      },
    ];
    expect(blocks).toHaveLength(3);
  });
});
