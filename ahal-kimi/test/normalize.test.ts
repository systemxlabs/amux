import { describe, expect, it } from "vitest";
import { KimiNormalizer, type KimiUpdate } from "../src/normalize.js";
import type { Event } from "ahal";

function upd(partial: Partial<KimiUpdate> & { sessionUpdate: string }): KimiUpdate {
  return partial as KimiUpdate;
}

function run(...updates: KimiUpdate[]): Event[] {
  const n = new KimiNormalizer();
  const out: Event[] = [];
  for (const u of updates) out.push(...n.push(u));
  return out;
}

function runAndFinish(stopReason: string, ...updates: KimiUpdate[]): Event[] {
  const n = new KimiNormalizer();
  const out: Event[] = [];
  for (const u of updates) out.push(...n.push(u));
  out.push(...n.finish(stopReason));
  return out;
}

describe("KimiNormalizer 三态映射与状态机", () => {
  it("完整工作区间：thinking → responding → acting → idle(end_turn)，idle 为最后一条", () => {
    const events = runAndFinish(
      "end_turn",
      upd({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "想" } }),
      upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "你" } }),
      upd({ sessionUpdate: "tool_call_update", toolCallId: "t1", toolName: "Bash", status: "in_progress" }),
      upd({ sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed" }),
    );
    const states = events
      .filter((e) => e.kind === "state_changed")
      .map((e) => (e as { state: string }).state);
    expect(states).toEqual(["thinking", "responding", "acting", "idle"]);
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "end_turn",
    });
  });

  it("chunk 无 messageId 时按种类合成稳定 id 并追加", () => {
    const events = run(
      upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "A" } }),
      upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "B" } }),
    );
    const chunks = events.filter((e) => e.kind === "agent_message_chunk");
    expect(chunks.map((c) => (c as { content: { text: string } }).content.text)).toEqual(["A", "B"]);
    const msg = events.find((e) => e.kind === "agent_message");
    expect(msg).toMatchObject({ messageId: "agent_message", content: [] });
  });

  it("带 messageId 的全量 agent_message 替换", () => {
    const events = run(
      upd({ sessionUpdate: "agent_message", messageId: "m1", content: [{ type: "text", text: "完整" }] }),
      upd({ sessionUpdate: "agent_message", messageId: "m1", content: [{ type: "text", text: "替换" }] }),
    );
    const msgs = events.filter((e) => e.kind === "agent_message");
    expect(msgs.map((m) => (m as { content: { text: string }[] }).content[0].text)).toEqual([
      "完整",
      "替换",
    ]);
  });

  it("工具调用：toolName/状态推进、content_chunk 追加", () => {
    const events = run(
      upd({ sessionUpdate: "tool_call_update", toolCallId: "t1", toolName: "Read", status: "in_progress" }),
      upd({ sessionUpdate: "tool_call_content_chunk", toolCallId: "t1", content: { type: "text", text: "out" } }),
      upd({ sessionUpdate: "tool_call_update", toolCallId: "t1", status: "failed" }),
    );
    const tools = events.filter((e) => e.kind === "tool_call_update");
    expect(tools[0]).toMatchObject({ toolCallId: "t1", toolName: "Read", status: "in_progress" });
    expect(tools[1]).toMatchObject({ toolCallId: "t1", status: "failed" });
    const chunks = events.filter((e) => e.kind === "tool_call_content_chunk");
    expect(chunks[0]).toMatchObject({ toolCallId: "t1" });
  });

  it("state_update idle 收尾并携带 stopReason；cancelled 透传", () => {
    const events = run(
      upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hi" } }),
      upd({ sessionUpdate: "state_update", state: "idle", stopReason: "cancelled" }),
    );
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "cancelled",
    });
  });

  it("收尾后：非忙碌类滞留事件被过滤；忙碌类事件视为新区间开始（kimi 无 turn 边界）", () => {
    const n = new KimiNormalizer();
    const all: Event[] = [];
    all.push(...n.push(upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "a" } })));
    all.push(...n.finish("end_turn"));
    // 滞留的非忙碌类通知（plan_update）被过滤
    all.push(...n.push(upd({ sessionUpdate: "plan_update", plan: { type: "items", entries: [] } })));
    expect(all.filter((e) => e.kind === "agent_message_chunk")).toHaveLength(1);
    // 忙碌类事件开启新区间
    all.push(...n.push(upd({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "新" } })));
    expect(all.filter((e) => e.kind === "agent_thought_chunk")).toHaveLength(1);
    // 新区间正常收尾
    all.push(...n.finish("end_turn"));
    expect(all[all.length - 1]).toEqual({ kind: "state_changed", state: "idle", reason: "end_turn" });
  });

  it("容忍未知 update 类型", () => {
    const events = run(
      upd({ sessionUpdate: "available_commands_update" }),
      upd({ sessionUpdate: "plan_update", plan: { type: "items", entries: [] } }),
      upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "ok" } }),
    );
    expect(events.some((e) => e.kind === "agent_message_chunk")).toBe(true);
  });
});
