import type { Event } from "ahal";
import { describe, expect, it } from "vitest";
import { EventBuffer } from "./eventbuffer.js";

function ev(seq: number, kind = "agent_message"): { seq: number; event: Event; timestamp: number } {
  return { seq, event: { kind, messageId: `m${seq}` } as unknown as Event, timestamp: 0 };
}

describe("EventBuffer（有界流式事件缓冲）", () => {
  it("按会话保留并淘汰最旧（有界）", () => {
    const b = new EventBuffer(3);
    b.push("s1", ev(1));
    b.push("s1", ev(2));
    b.push("s1", ev(3));
    b.push("s1", ev(4));
    expect(b.get("s1").map((e) => e.seq)).toEqual([2, 3, 4]);
  });

  it("不同会话相互隔离", () => {
    const b = new EventBuffer(10);
    b.push("a", ev(1));
    b.push("b", ev(1));
    expect(b.get("a").map((e) => e.seq)).toEqual([1]);
    expect(b.get("b").map((e) => e.seq)).toEqual([1]);
  });

  it("get(afterSeq) 只返回缺口之后的（重连补齐语义）", () => {
    const b = new EventBuffer(10);
    for (let i = 1; i <= 5; i++) b.push("s", ev(i));
    expect(b.get("s", 3).map((e) => e.seq)).toEqual([4, 5]);
    expect(b.get("s", 99)).toEqual([]);
  });

  it("clear 清空该会话", () => {
    const b = new EventBuffer(10);
    b.push("s", ev(1));
    b.clear("s");
    expect(b.get("s")).toEqual([]);
  });

  it("未知会话返回空", () => {
    const b = new EventBuffer(10);
    expect(b.get("nope")).toEqual([]);
  });
});
