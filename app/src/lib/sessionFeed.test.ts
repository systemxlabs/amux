import { describe, expect, it } from "vitest";
import { SessionFeed } from "./sessionFeed.js";

const ev = (kind = "agent_message") => ({ event: { kind, messageId: "m" } as never, timestamp: 0 });
const userMsg = () => ({ content: [{ type: "text" as const, text: "用户" }], timestamp: 0 });

describe("SessionFeed（历史 + 实时按序追加）", () => {
  it("applyHistory 以历史替换；实时项按到达顺序追加（无去重逻辑）", () => {
    const feed = new SessionFeed("s1");
    feed.applyHistory([ev("agent_thought"), userMsg(), ev("agent_message")]);
    expect(feed.events).toHaveLength(3);
    feed.apply([ev("agent_thought"), userMsg()]);
    expect(feed.events).toHaveLength(5);
    feed.applyOne(ev("agent_message"));
    expect(feed.events).toHaveLength(6);
    expect(feed.revision).toBe(4); // applyHistory 1 + apply 2 条 + applyOne 1 条
  });

  it("drainNotify：只含以通知（apply/applyOne）到达的实时项；消费即清空", () => {
    const feed = new SessionFeed("s1");
    feed.applyHistory([ev("agent_thought")]); // 历史不触发通知
    expect(feed.drainNotify()).toEqual([]);
    feed.applyOne(ev("agent_message"));
    feed.applyOne(userMsg());
    expect(feed.drainNotify()).toHaveLength(2);
    expect(feed.drainNotify()).toEqual([]); // 已消费
  });

  it("apply 与 applyOne 同语义：按序追加、返回新增数", () => {
    const feed = new SessionFeed("s1");
    expect(feed.apply([ev(), ev()])).toBe(2);
    expect(feed.events).toHaveLength(2);
    expect(feed.applyOne(ev())).toBe(true);
    expect(feed.events).toHaveLength(3);
  });
});
