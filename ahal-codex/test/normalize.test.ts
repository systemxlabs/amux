import { describe, expect, it } from "vitest";
import { CodexNormalizer, type CodexWireEvent } from "../src/normalize.js";
import type { Event } from "ahal";

const TID = "thread-1";

function ev(method: string, params: unknown): CodexWireEvent {
  return { method, params } as unknown as CodexWireEvent;
}

/** 工具函数：把事件序列压入归一化器，展平返回全部 AHAL 事件 */
function run(...wires: CodexWireEvent[]): Event[] {
  const n = new CodexNormalizer();
  const out: Event[] = [];
  for (const w of wires) out.push(...n.push(w));
  return out;
}

describe("CodexNormalizer 三态映射与状态机", () => {
  it("完整工作区间：thinking → responding → acting → idle，idle 为最后一条且带结束原因", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("reasoningTextDelta", { threadId: TID, turnId: "t1", itemId: "r1", delta: "思考中" }),
      ev("item/started", {
        threadId: TID,
        turnId: "t1",
        item: { id: "m1", type: "agentMessage", text: "" },
      }),
      ev("item/agentMessage/delta", { threadId: TID, turnId: "t1", itemId: "m1", delta: "你好" }),
      ev("item/started", {
        threadId: TID,
        turnId: "t1",
        item: { id: "c1", type: "commandExecution", command: "ls" },
      }),
      ev("item/completed", {
        threadId: TID,
        turnId: "t1",
        item: { id: "c1", type: "commandExecution", command: "ls", status: "completed", exitCode: 0 },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "completed" } }),
    );

    const kinds = events.map((e) => e.kind);
    // 状态迁移依次：thinking、responding（消息）、acting（工具）、idle 收尾
    expect(kinds.filter((k) => k === "state_changed")).toEqual([
      "state_changed",
      "state_changed",
      "state_changed",
      "state_changed",
    ]);
    const states = events
      .filter((e) => e.kind === "state_changed")
      .map((e) => (e as { state: string }).state);
    expect(states).toEqual(["thinking", "responding", "acting", "idle"]);
    // idle 是最后一条，带 end_turn
    const last = events[events.length - 1];
    expect(last).toEqual({ kind: "state_changed", state: "idle", reason: "end_turn" });
    // idle 之后无任何事件
    expect(events.indexOf(last)).toBe(events.length - 1);
  });

  it("thinking 与 responding 交替（消息交错），按 ID 聚合、chunk 追加", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("item/started", {
        threadId: TID,
        turnId: "t1",
        item: { id: "m1", type: "agentMessage", text: "A" },
      }),
      ev("item/agentMessage/delta", { threadId: TID, turnId: "t1", itemId: "m1", delta: "B" }),
      ev("item/agentMessage/delta", { threadId: TID, turnId: "t1", itemId: "m1", delta: "C" }),
      ev("reasoningTextDelta", { threadId: TID, turnId: "t1", itemId: "r1", delta: "想" }),
      ev("item/completed", {
        threadId: TID,
        turnId: "t1",
        item: { id: "m1", type: "agentMessage", text: "ABC" },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "completed" } }),
    );
    // 消息创建：agent_message 出现一次（started），chunk 追加两条，completed 整体替换
    const msgs = events.filter((e) => e.kind === "agent_message");
    expect(msgs).toHaveLength(2);
    expect(msgs[0]).toMatchObject({ messageId: "m1", content: [{ type: "text", text: "A" }] });
    expect(msgs[1]).toMatchObject({ messageId: "m1", content: [{ type: "text", text: "ABC" }] });
    const chunks = events.filter((e) => e.kind === "agent_message_chunk");
    expect(chunks.map((c) => (c as { content: { text: string } }).content.text)).toEqual([
      "B",
      "C",
    ]);
    // 交错：thinking 出现在 responding 之后，触发 responding → thinking 迁移
    const thoughtChunks = events.filter((e) => e.kind === "agent_thought_chunk");
    expect(thoughtChunks.map((c) => (c as { content: { text: string } }).content.text)).toEqual([
      "想",
    ]);
  });

  it("工具调用状态推进：in_progress → completed / failed（按 exitCode）", () => {
    const ok = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("item/started", {
        threadId: TID,
        turnId: "t1",
        item: { id: "c1", type: "commandExecution", command: "ls" },
      }),
      ev("item/completed", {
        threadId: TID,
        turnId: "t1",
        item: { id: "c1", type: "commandExecution", status: "completed", exitCode: 0 },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "completed" } }),
    );
    const okTools = ok.filter((e) => e.kind === "tool_call_update");
    expect(okTools[0]).toMatchObject({ toolCallId: "c1", status: "in_progress", toolName: "command" });
    expect(okTools[1]).toMatchObject({ toolCallId: "c1", status: "completed" });

    const fail = run(
      ev("turn/started", { threadId: TID, turn: { id: "t2", status: "inProgress" } }),
      ev("item/started", {
        threadId: TID,
        turnId: "t2",
        item: { id: "c2", type: "commandExecution", command: "rm x" },
      }),
      ev("item/completed", {
        threadId: TID,
        turnId: "t2",
        item: { id: "c2", type: "commandExecution", status: "failed", exitCode: 1 },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t2", status: "completed" } }),
    );
    const failTools = fail.filter((e) => e.kind === "tool_call_update");
    expect(failTools[1]).toMatchObject({ toolCallId: "c2", status: "failed" });
  });

  it("取消：turn/completed cancelled → idle(cancelled)", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("item/started", {
        threadId: TID,
        turnId: "t1",
        item: { id: "m1", type: "agentMessage", text: "" },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "cancelled" } }),
    );
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "cancelled",
    });
  });

  it("异常：turn errored → 先发 error 事件，再 idle(error)", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("turn/completed", {
        threadId: TID,
        turn: { id: "t1", status: "errored", error: { message: "boom" } },
      }),
    );
    expect(events).toContainEqual({ kind: "error", message: '{"message":"boom"}' });
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "error",
    });
  });

  it("usage_update 独立事件", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("thread/tokenUsage/updated", {
        threadId: TID,
        turnId: "t1",
        tokenUsage: { total: { totalTokens: 100, inputTokens: 80, outputTokens: 20 }, modelContextWindow: 200000 },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "completed" } }),
    );
    expect(events).toContainEqual({
      kind: "usage_update",
      context: 100,
      contextWindow: 200000,
    });
  });

  it("idle 后过滤上一区间滞留事件，新区间正常开始", () => {
    const n = new CodexNormalizer();
    const seq: CodexWireEvent[] = [
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("item/started", {
        threadId: TID,
        turnId: "t1",
        item: { id: "m1", type: "agentMessage", text: "" },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "completed" } }),
      // 滞留：上一区间的消息 delta，应被过滤
      ev("item/agentMessage/delta", { threadId: TID, turnId: "t1", itemId: "m1", delta: "滞留" }),
      // 新区间
      ev("turn/started", { threadId: TID, turn: { id: "t2", status: "inProgress" } }),
      ev("item/started", {
        threadId: TID,
        turnId: "t2",
        item: { id: "m2", type: "agentMessage", text: "新" },
      }),
      ev("turn/completed", { threadId: TID, turn: { id: "t2", status: "completed" } }),
    ];
    const all: Event[] = [];
    for (const w of seq) all.push(...n.push(w));
    // 滞留 delta 不出现
    expect(all.filter((e) => e.kind === "agent_message_chunk")).toHaveLength(0);
    // 两个区间各以一个 idle 收尾
    const idles = all.filter((e) => e.kind === "state_changed" && e.state === "idle");
    expect(idles).toHaveLength(2);
    // 新区间的消息出现
    expect(all.some((e) => e.kind === "agent_message" && e.messageId === "m2")).toBe(true);
  });

  it("容忍未知通知类型（不抛错、不产生事件）", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("future/notification", { threadId: TID, some: "unknown" }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "completed" } }),
    );
    expect(events.length).toBeGreaterThan(0);
    expect(events.filter((e) => e.kind === "state_changed")).toHaveLength(2);
  });
  it("interrupted 状态映射为 cancelled（codex interrupt 后状态）", () => {
    const events = run(
      ev("turn/started", { threadId: TID, turn: { id: "t1", status: "inProgress" } }),
      ev("turn/completed", { threadId: TID, turn: { id: "t1", status: "interrupted" } }),
    );
    expect(events[events.length - 1]).toEqual({
      kind: "state_changed",
      state: "idle",
      reason: "cancelled",
    });
  });
});
