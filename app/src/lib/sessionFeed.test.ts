import { describe, expect, it } from "vitest";
import { SessionFeed } from "./sessionFeed.js";

const ev = (seq: number, kind = "agent_message") => ({
  seq,
  event: { kind, messageId: `m${seq}` } as never,
  timestamp: seq,
});

describe("SessionFeed（重连补齐合并）", () => {
  it("历史替换 + 缺口补齐去重（恰好一次、不重复）", () => {
    const feed = new SessionFeed("s1");
    feed.applyHistory([ev(1), ev(2), ev(3)]);
    expect(feed.lastSeq).toBe(3);
    // 缓冲带回与历史重叠的事件（2、3）与缺口事件（4、5）
    const added = feed.apply([ev(2), ev(3), ev(4), ev(5)]);
    expect(added).toBe(2);
    expect(feed.events.map((e) => e.seq)).toEqual([1, 2, 3, 4, 5]);
  });

  it("实时乱序/重复事件被丢弃", () => {
    const feed = new SessionFeed("s1");
    feed.applyHistory([ev(1), ev(2)]);
    expect(feed.applyOne(ev(2))).toBe(false); // 重复
    expect(feed.applyOne(ev(1))).toBe(false); // 乱序旧事件
    expect(feed.applyOne(ev(3))).toBe(true);
    expect(feed.events.map((e) => e.seq)).toEqual([1, 2, 3]);
  });

  it("空历史时 seen = -1，缓冲全部接受", () => {
    const feed = new SessionFeed("s1");
    feed.applyHistory([]);
    expect(feed.lastSeq).toBe(-1);
    expect(feed.apply([ev(1), ev(2)])).toBe(2);
  });
});
