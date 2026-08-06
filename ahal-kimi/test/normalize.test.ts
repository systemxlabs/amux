import { describe, expect, it } from "vitest";
import { KimiSdkNormalizer, type KimiSdkEvent } from "../src/normalize.js";
import type { Event } from "ahal";

function ev(partial: Record<string, unknown> & { type: string }): KimiSdkEvent {
  return partial as unknown as KimiSdkEvent;
}

function run(...events: KimiSdkEvent[]): Event[] {
  const n = new KimiSdkNormalizer();
  const out: Event[] = [];
  for (const e of events) out.push(...n.push(e));
  return out;
}

function runAndFinish(reason: string, ...events: KimiSdkEvent[]): Event[] {
  const n = new KimiSdkNormalizer();
  const out: Event[] = [];
  for (const e of events) out.push(...n.push(e));
  out.push(...n.finish(reason as never));
  return out;
}

describe("KimiSdkNormalizer 三态映射与状态机", () => {
  it("完整工作区间：thinking → responding → acting → idle(end_turn)，idle 为最后一条", () => {
    const events = runAndFinish(
      "end_turn",
      ev({ type: "turn.started", turnId: 0 }),
      ev({ type: "thinking.delta", turnId: 0, delta: "想" }),
      ev({ type: "assistant.delta", turnId: 0, delta: "你" }),
      ev({ type: "tool.call.started", turnId: 0, toolCallId: "t1", name: "Read" }),
      ev({ type: "tool.result", turnId: 0, toolCallId: "t1", isError: false }),
    );
    const states = events
      .filter((e) => e.kind === "state_changed")
      .map((e) => (e as { state: string }).state);
    expect(states).toEqual(["thinking", "responding", "acting", "thinking", "idle"]);
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "end_turn",
    });
  });

  it("turn.ended 映射结束原因：completed / cancelled / failed / blocked", () => {
    const mk = (reason: string) =>
      run(
        ev({ type: "turn.started", turnId: 0 }),
        ev({ type: "turn.ended", turnId: 0, reason }),
      );
    expect((mk("completed").at(-1) as { reason?: string }).reason).toBe("end_turn");
    expect((mk("cancelled").at(-1) as { reason?: string }).reason).toBe("cancelled");
    const failed = mk("failed");
    expect(failed.some((e) => e.kind === "error")).toBe(true);
    expect((failed.at(-1) as { reason?: string }).reason).toBe("error");
    expect((mk("blocked").at(-1) as { reason?: string }).reason).toBe("refusal");
  });

  it("文本/思考按种类合成稳定 id 并追加 chunk", () => {
    const events = run(
      ev({ type: "turn.started", turnId: 0 }),
      ev({ type: "assistant.delta", turnId: 0, delta: "A" }),
      ev({ type: "assistant.delta", turnId: 0, delta: "B" }),
      ev({ type: "thinking.delta", turnId: 0, delta: "想" }),
    );
    const chunks = events.filter((e) => e.kind === "agent_message_chunk");
    expect(chunks.map((c) => (c as { content: { text: string } }).content.text)).toEqual(["A", "B"]);
    const thought = events.filter((e) => e.kind === "agent_thought_chunk");
    expect(thought.map((c) => (c as { content: { text: string } }).content.text)).toEqual(["想"]);
  });

  it("工具调用：toolName/状态推进、isError → failed", () => {
    const events = run(
      ev({ type: "turn.started", turnId: 0 }),
      ev({ type: "tool.call.started", turnId: 0, toolCallId: "t1", name: "Read", description: "读文件" }),
      ev({ type: "tool.result", turnId: 0, toolCallId: "t1", isError: false }),
      ev({ type: "tool.call.started", turnId: 0, toolCallId: "t2", name: "Bash" }),
      ev({ type: "tool.result", turnId: 0, toolCallId: "t2", isError: true }),
    );
    const tools = events.filter((e) => e.kind === "tool_call_update");
    expect(tools[0]).toMatchObject({ toolCallId: "t1", toolName: "Read", status: "in_progress", title: "读文件" });
    expect(tools[1]).toMatchObject({ toolCallId: "t1", status: "completed" });
    expect(tools[2]).toMatchObject({ toolCallId: "t2", toolName: "Bash", status: "in_progress" });
    expect(tools[3]).toMatchObject({ toolCallId: "t2", status: "failed" });
  });

  it("agent.status.updated → usage_update", () => {
    const events = run(
      ev({ type: "turn.started", turnId: 0 }),
      ev({ type: "agent.status.updated", contextTokens: 1234, maxContextTokens: 262144 }),
    );
    expect(events).toContainEqual({ kind: "usage_update", context: 1234, contextWindow: 262144 });
  });

  it("error 事件透传", () => {
    const events = run(ev({ type: "error", message: "boom", code: "x" }));
    expect(events).toContainEqual({ kind: "error", message: "boom" });
  });

  it("收尾后：非忙碌类事件过滤，忙碌类开启新区间", () => {
    const n = new KimiSdkNormalizer();
    const all: Event[] = [];
    all.push(...n.push(ev({ type: "turn.started", turnId: 0 })));
    all.push(...n.finish("end_turn"));
    // 非忙碌事件（如状态更新之外的未知类型）被过滤
    all.push(...n.push(ev({ type: "session.meta.updated", sessionId: "s" })));
    expect(all.filter((e) => e.kind === "state_changed")).toHaveLength(2);
    // 忙碌类事件开启新区间
    all.push(...n.push(ev({ type: "turn.started", turnId: 1 })));
    all.push(...n.push(ev({ type: "assistant.delta", turnId: 1, delta: "新" })));
    expect(all.filter((e) => e.kind === "agent_message_chunk")).toHaveLength(1);
  });

  it("容忍未知事件类型", () => {
    const events = run(
      ev({ type: "turn.started", turnId: 0 }),
      ev({ type: "hook.result", hook: "x" }),
      ev({ type: "shell.output", output: "x" }),
      ev({ type: "turn.ended", turnId: 0, reason: "completed" }),
    );
    expect((events.at(-1) as { reason?: string }).reason).toBe("end_turn");
  });
});
