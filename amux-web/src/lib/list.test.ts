// 会话列表统一排序与工作流关联会话的折叠展开（docs/PRD.md「会话列表视图」）。

import { describe, expect, it } from "vitest";

import { buildListWindow, canExpand, listRows, sortEntries } from "./list";
import { newPaging } from "./paging";
import type { ListEntry, Session, Workflow } from "./types";

function session(id: string, updatedAt: number, state: Session["state"] = "idle"): Session {
  return {
    id,
    machine: "localpc",
    agent: "codex",
    title: id,
    state,
    workspace: "/w",
    worktreeDir: "",
    createdAt: 1,
    updatedAt,
  };
}

function workflow(id: string, updatedAt: number, linked: Session[]): Workflow {
  return {
    id,
    title: id,
    state: "idle",
    plan: "计划",
    createdAt: 1,
    updatedAt,
    linkedSessions: linked,
  };
}

describe("sortEntries", () => {
  it("普通会话与工作流会话统一按最近活跃倒序", () => {
    const entries: ListEntry[] = [
      { kind: "session", session: session("s-old", 100) },
      { kind: "workflow", workflow: workflow("w", 300, []) },
      { kind: "session", session: session("s-new", 500) },
    ];
    expect(sortEntries(entries).map((entry) => (entry.kind === "session" ? entry.session.id : entry.workflow.id))).toEqual([
      "s-new",
      "w",
      "s-old",
    ]);
  });
});

describe("buildListWindow", () => {
  it("两个来源构建为统一排序的最新窗口，并更新更早标记", () => {
    const result = buildListWindow(
      newPaging(),
      [session("s1", 100)],
      [workflow("w1", 200, [session("linked", 150)])],
      true,
    );
    expect(result.entries.map((entry) => (entry.kind === "session" ? entry.session.id : entry.workflow.id))).toEqual([
      "w1",
      "s1",
    ]);
    expect(result.paging.hasOlder).toBe(true);
  });

  it("重建窗口不保留服务端已删除的旧条目", () => {
    const result = buildListWindow(
      newPaging(),
      [session("alive", 300)],
      [workflow("kept", 200, [])],
      false,
    );
    expect(result.entries.map((entry) => (entry.kind === "session" ? entry.session.id : entry.workflow.id))).toEqual([
      "alive",
      "kept",
    ]);
  });
});

describe("listRows", () => {
  const linked = [session("l-low", 150), session("l-high", 400)];
  const entries: ListEntry[] = [
    { kind: "workflow", workflow: workflow("w", 300, linked) },
    { kind: "session", session: session("s", 200) },
  ];

  it("默认折叠：工作流会话只占一行", () => {
    const rows = listRows(entries, new Set());
    expect(rows.map((row) => (row.entry.kind === "session" ? row.entry.session.id : row.entry.workflow.id))).toEqual(
      ["w", "s"],
    );
    expect(rows.every((row) => row.depth === 0)).toBe(true);
  });

  it("展开后关联会话按自身最近活跃倒序紧随其后，且不影响其他条目位置", () => {
    const rows = listRows(entries, new Set(["w"]));
    expect(
      rows.map((row) => ({
        id: row.entry.kind === "session" ? row.entry.session.id : row.entry.workflow.id,
        depth: row.depth,
      })),
    ).toEqual([
      { id: "w", depth: 0 },
      { id: "l-high", depth: 1 },
      { id: "l-low", depth: 1 },
      { id: "s", depth: 0 },
    ]);
  });

  it("无关联会话的工作流会话不可展开", () => {
    expect(canExpand({ kind: "workflow", workflow: workflow("w", 1, []) })).toBe(false);
  });
});
