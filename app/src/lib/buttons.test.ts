import { describe, expect, it } from "vitest";
import { buttonDisabled, DEFAULT_BUTTONS, type ActionButton } from "./buttons.js";

const btn = (over: Partial<ActionButton> & Pick<ActionButton, "kind">): ActionButton => ({
  id: "x",
  label: "x",
  enabled: true,
  ...over,
});

describe("DEFAULT_BUTTONS（按钮栏集合）", () => {
  it("预设：Commit & Push（合并）、Submit PR；会话级操作（new/kill/delete）在侧边栏", () => {
    expect(DEFAULT_BUTTONS.map((b) => b.id)).toEqual(["commit-push", "submit-pr"]);
    const cp = DEFAULT_BUTTONS[0];
    expect(cp.kind).toBe("prompt");
    expect(cp.promptTemplate).toContain("push");
  });
});

describe("buttonDisabled（按钮可用性）", () => {
  it("prompt 型需要会话可用", () => {
    const commit = btn({ id: "commit", kind: "prompt" });
    expect(buttonDisabled(commit, "thinking", false, false)).toBe(false); // 忙时可用（steer）
    expect(buttonDisabled(commit, "idle", true, false)).toBe(true);
    expect(buttonDisabled(commit, "idle", false, true)).toBe(true);
  });

  it("push 需要会话可用", () => {
    const push = btn({ id: "push", kind: "git-push" });
    expect(buttonDisabled(push, "acting", false, false)).toBe(false);
    expect(buttonDisabled(push, "idle", false, true)).toBe(true);
  });

  it("delete 始终可用（永久删除，客户端发起）", () => {
    const del = btn({ id: "delete", kind: "delete-session" });
    expect(buttonDisabled(del, "thinking", false, false)).toBe(false);
    expect(buttonDisabled(del, "idle", true, false)).toBe(false);
    expect(buttonDisabled(del, "idle", false, true)).toBe(false);
  });

  it("禁用开关生效", () => {
    const off = btn({ id: "commit", kind: "prompt", enabled: false });
    expect(buttonDisabled(off, "idle", false, false)).toBe(true);
  });
});
