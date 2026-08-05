import { describe, expect, it } from "vitest";
import { ClaudeNormalizer, type ClaudeMessage, type ClaudeRawStreamEvent } from "../src/normalize.js";
import type { Event } from "ahal";

function streamEvent(event: ClaudeRawStreamEvent): ClaudeMessage {
  return { type: "stream_event", event, session_id: "s1" };
}
function user(content: unknown): ClaudeMessage {
  return { type: "user", message: { role: "user", content } as never, session_id: "s1" };
}
function result(subtype: "success" | "error", extra?: Record<string, unknown>): ClaudeMessage {
  return { type: "result", subtype, session_id: "s1", stop_reason: "end_turn", ...extra } as ClaudeMessage;
}

function run(...msgs: ClaudeMessage[]): Event[] {
  const n = new ClaudeNormalizer();
  const out: Event[] = [];
  for (const m of msgs) out.push(...n.push(m));
  return out;
}

describe("ClaudeNormalizer 三态映射与状态机", () => {
  it("完整工作区间：thinking → responding → acting → idle(end_turn)，idle 为最后一条", () => {
    const events = run(
      streamEvent({ type: "content_block_start", index: 0, content_block: { type: "thinking" } }),
      streamEvent({
        type: "content_block_delta",
        index: 0,
        delta: { type: "thinking_delta", thinking: "想" },
      }),
      streamEvent({ type: "content_block_start", index: 1, content_block: { type: "text" } }),
      streamEvent({ type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "你" } }),
      streamEvent({
        type: "content_block_start",
        index: 2,
        content_block: { type: "tool_use", id: "tu1", name: "Bash" },
      }),
      streamEvent({
        type: "content_block_delta",
        index: 2,
        delta: { type: "input_json_delta", partial_json: "{}" },
      }),
      user([{ type: "tool_result", tool_use_id: "tu1", is_error: false, content: "ok" }]),
      result("success"),
    );

    const states = events
      .filter((e) => e.kind === "state_changed")
      .map((e) => (e as { state: string }).state);
    // tool_result 后模型回到思考 → 最后一段是 thinking → idle
    expect(states).toEqual(["thinking", "responding", "acting", "thinking", "idle"]);
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "end_turn",
    });
  });

  it("文本块：创建 → chunk 追加；工具结果把工具置为终态", () => {
    const events = run(
      streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
      streamEvent({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "A" } }),
      streamEvent({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "B" } }),
      streamEvent({
        type: "content_block_start",
        index: 1,
        content_block: { type: "tool_use", id: "tu1", name: "Read" },
      }),
      user([{ type: "tool_result", tool_use_id: "tu1", is_error: true, content: "err" }]),
      result("success"),
    );
    const chunks = events.filter((e) => e.kind === "agent_message_chunk");
    expect(chunks.map((c) => (c as { content: { text: string } }).content.text)).toEqual(["A", "B"]);
    const tools = events.filter((e) => e.kind === "tool_call_update");
    expect(tools[0]).toMatchObject({ toolCallId: "tu1", toolName: "Read", status: "in_progress" });
    expect(tools[1]).toMatchObject({ toolCallId: "tu1", status: "failed" });
  });

  it("thinking_tokens 系统消息触发 thinking", () => {
    const events = run(
      { type: "system", subtype: "thinking_tokens", session_id: "s1" },
      streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text" } }),
      result("success"),
    );
    const states = events
      .filter((e) => e.kind === "state_changed")
      .map((e) => (e as { state: string }).state);
    expect(states[0]).toBe("thinking");
    expect(states[1]).toBe("responding");
  });

  it("max_tokens 结束原因透传", () => {
    const events = run(
      streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text" } }),
      { type: "result", subtype: "success", stop_reason: "max_tokens", session_id: "s1" } as ClaudeMessage,
    );
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "max_tokens",
    });
  });

  it("错误：result error → error 事件 + idle(error)", () => {
    const events = run(
      streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text" } }),
      result("error", { error: "boom" }),
    );
    expect(events).toContainEqual({ kind: "error", message: "boom" });
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "error",
    });
  });

  it("finish(cancelled)：外部收尾；收尾后忙碌类消息开启新区间，非忙碌类被过滤", () => {
    const n = new ClaudeNormalizer();
    const events: Event[] = [];
    events.push(
      ...n.push(
        streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text" } }),
      ),
    );
    events.push(...n.finish("cancelled"));
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "cancelled",
    });
    // 收尾后：非忙碌类消息（如 result 之外的未知流事件）被过滤
    events.push(...n.push(streamEvent({ type: "message_stop" })));
    expect(events.filter((e) => e.kind === "agent_message_chunk")).toHaveLength(0);
    // 忙碌类消息开启新区间（多 prompt 会话：下一轮的消息继续流入）
    events.push(
      ...n.push(
        streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text" } }),
      ),
    );
    events.push(
      ...n.push(
        streamEvent({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "x" } }),
      ),
    );
    expect(events.filter((e) => e.kind === "agent_message_chunk")).toHaveLength(1);
  });

  it("整条 assistant 消息：thinking/text/tool_use 块映射（SDK 默认路径）", () => {
    const events = run(
      {
        type: "assistant",
        message: {
          id: "m1",
          content: [
            { type: "thinking", thinking: "想一下" },
            { type: "text", text: "hello" },
            { type: "tool_use", id: "tu9", name: "Bash", input: {} },
          ],
        },
        session_id: "s1",
      } as ClaudeMessage,
      user([{ type: "tool_result", tool_use_id: "tu9", is_error: false, content: "ok" }]),
      result("success"),
    );
    const states = events
      .filter((e) => e.kind === "state_changed")
      .map((e) => (e as { state: string }).state);
    expect(states).toEqual(["thinking", "responding", "acting", "thinking", "idle"]);
    const msgs = events.filter((e) => e.kind === "agent_message");
    expect(msgs[0]).toMatchObject({
      messageId: "m1#1",
      content: [{ type: "text", text: "hello" }],
    });
    const thoughts = events.filter((e) => e.kind === "agent_thought");
    expect(thoughts[0]).toMatchObject({
      messageId: "m1#0",
      content: [{ type: "text", text: "想一下" }],
    });
    const tools = events.filter((e) => e.kind === "tool_call_update");
    expect(tools[0]).toMatchObject({ toolCallId: "tu9", toolName: "Bash", status: "in_progress" });
    expect(tools[1]).toMatchObject({ toolCallId: "tu9", status: "completed" });
  });

  it("容忍未知消息类型（不抛错、正常收尾）", () => {
    const events = run(
      streamEvent({ type: "content_block_start", index: 0, content_block: { type: "text" } }),
      streamEvent({ type: "ping" }),
      streamEvent({ type: "message_stop" }),
      { type: "unknown_kind" } as unknown as ClaudeMessage,
      result("success"),
    );
    // 未知消息不产生事件但也不中断流；正常以 idle 收尾
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "end_turn",
    });
  });
});
