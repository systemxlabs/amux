import { describe, expect, it } from "vitest";
import { buttonDisabled, DEFAULT_BUTTONS } from "./buttons.js";

describe("buttonDisabled（按钮可用性）", () => {
  const byId = (id: string) => DEFAULT_BUTTONS.find((b) => b.id === id)!;

  it("undo/revert 需等工作区间结束：忙时禁用、空闲可用", () => {
    const undo = byId("undo");
    expect(buttonDisabled(undo, "thinking", false, false)).toBe(true);
    expect(buttonDisabled(undo, "acting", false, false)).toBe(true);
    expect(buttonDisabled(undo, "idle", false, false)).toBe(false);
    expect(buttonDisabled(undo, "idle", true, false)).toBe(true); // 已关闭
    expect(buttonDisabled(undo, "idle", false, true)).toBe(true); // 已中断
  });

  it("prompt 型需要会话可用", () => {
    const commit = byId("commit");
    expect(buttonDisabled(commit, "thinking", false, false)).toBe(false); // 忙时可用（steer）
    expect(buttonDisabled(commit, "idle", true, false)).toBe(true);
    expect(buttonDisabled(commit, "idle", false, true)).toBe(true);
  });

  it("push 需要会话可用", () => {
    const push = byId("push");
    expect(buttonDisabled(push, "acting", false, false)).toBe(false);
    expect(buttonDisabled(push, "idle", false, true)).toBe(true);
  });

  it("kill / new-session 始终可用", () => {
    expect(buttonDisabled(byId("kill"), "thinking", false, false)).toBe(false);
    expect(buttonDisabled(byId("kill"), "idle", true, false)).toBe(false);
    expect(buttonDisabled(byId("new-session"), "idle", false, true)).toBe(false);
  });
});
