import { describe, expect, it } from "vitest";
import { NotificationDetector } from "./notify.js";

describe("NotificationDetector（通知推导）", () => {
  it("忙 → 空闲（工作结束）时通知并携带结束原因", () => {
    const d = new NotificationDetector({ workEnded: true, onError: false, longIdleSeconds: 0 });
    expect(d.onEvent({ kind: "state_changed", state: "thinking" }, 100)).toBeNull();
    expect(d.onEvent({ kind: "state_changed", state: "idle", reason: "end_turn" }, 200)).toEqual({
      kind: "work-ended",
      reason: "end_turn",
    });
  });

  it("未处于忙状态时不发工作结束通知", () => {
    const d = new NotificationDetector({ workEnded: true, onError: false, longIdleSeconds: 0 });
    expect(d.onEvent({ kind: "state_changed", state: "idle", reason: "end_turn" }, 100)).toBeNull();
  });

  it("error 事件按配置通知", () => {
    const on = new NotificationDetector({ workEnded: false, onError: true, longIdleSeconds: 0 });
    expect(on.onEvent({ kind: "error", message: "boom" }, 100)).toEqual({ kind: "error", message: "boom" });
    const off = new NotificationDetector({ workEnded: false, onError: false, longIdleSeconds: 0 });
    expect(off.onEvent({ kind: "error", message: "boom" }, 100)).toBeNull();
  });

  it("长时间无响应：超过阈值且忙时通知，防刷", () => {
    let now = 1000;
    const d = new NotificationDetector({ workEnded: false, onError: false, longIdleSeconds: 10 }, () => now);
    d.onEvent({ kind: "state_changed", state: "acting" }, 1000);
    expect(d.tick(1050)).toBeNull(); // 未到 10s
    expect(d.tick(11001)).toEqual({ kind: "long-idle" });
    expect(d.tick(11500)).toBeNull(); // 防刷
    d.onEvent({ kind: "agent_message", messageId: "m", content: [{ type: "text", text: "x" }] }, 12000);
    expect(d.tick(22100)).toEqual({ kind: "long-idle" }); // 有事件后重新计时
  });

  it("longIdleSeconds=0 时关闭", () => {
    let now = 0;
    const d = new NotificationDetector({ workEnded: false, onError: false, longIdleSeconds: 0 }, () => now);
    d.onEvent({ kind: "state_changed", state: "acting" }, 0);
    expect(d.tick(99999)).toBeNull();
  });
});
