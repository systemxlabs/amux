import { describe, expect, it } from "vitest";
import { ConnectionCatchup } from "./catchup.js";

const item = (order: number, kind: "event" | "user_message" = "event") => ({ order, kind, params: { sessionId: "s1" } });

describe("ConnectionCatchup（按连接补齐纯逻辑）", () => {
  it("补齐前到达的会话流通知暂存；get_history 标记位置后返回缺口项（恰好一次、按序）", () => {
    const c = new ConnectionCatchup();
    // 连接建立后、get_history 前：3 条实时项（含 1 条已含在历史快照中的）
    expect(c.route("s1", 1, "event", { n: 1 })).toBe("stash");
    expect(c.route("s1", 2, "event", { n: 2 })).toBe("stash");
    expect(c.route("s1", 3, "user_message", { n: 3 })).toBe("stash");

    // get_history：历史快照含 order<=2 的记录，位置标记为 2 → 只补发缺口（3）
    const pendings = c.mark("s1", 2);
    expect(pendings.map((p) => p.order)).toEqual([3]);
    expect(pendings[0].method).toBe("user_message");
  });

  it("补齐后实时项直接 deliver；旧序号项 skip（历史已包含，防御）", () => {
    const c = new ConnectionCatchup();
    c.mark("s1", 5);
    expect(c.route("s1", 6, "event", { n: 6 })).toBe("deliver");
    expect(c.route("s1", 5, "event", { n: 5 })).toBe("skip");
    expect(c.route("s1", 4, "event", { n: 4 })).toBe("skip");
  });

  it("未补齐的会话持续暂存，与其他会话互不影响", () => {
    const c = new ConnectionCatchup();
    c.mark("s1", 0);
    expect(c.route("s1", 1, "event", { n: 1 })).toBe("deliver");
    expect(c.route("s2", 1, "event", { n: 1 })).toBe("stash"); // s2 未补齐
    expect(c.route("s2", 2, "event", { n: 2 })).toBe("stash");
    const pendings = c.mark("s2", 1);
    expect(pendings.map((p) => p.order)).toEqual([2]); // 缺口恰好一次
    expect(c.route("s2", 3, "event", { n: 3 })).toBe("deliver");
  });

  it("clear 释放全部状态（连接关闭）", () => {
    const c = new ConnectionCatchup();
    c.route("s1", 1, "event", { n: 1 });
    c.mark("s1", 0);
    expect(c.route("s1", 2, "event", { n: 2 })).toBe("deliver");
    c.clear();
    // 清空后视同新连接：s1 未补齐 → stash
    expect(c.route("s1", 3, "event", { n: 3 })).toBe("stash");
  });

  it("暂存超出防御上限时丢弃最旧（客户端永不 get_history 的异常场景）", () => {
    const c = new ConnectionCatchup();
    for (let i = 1; i <= 5002; i++) c.route("s1", i, "event", { n: i });
    const pendings = c.mark("s1", 0);
    expect(pendings.length).toBe(5000);
    expect(pendings[0].order).toBe(3); // 最旧两条被丢弃，缺口从 3 开始
  });
});
