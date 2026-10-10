// 会话列表统一排序与工作流关联会话的折叠展开（docs/PRD.md「会话列表视图」）。

import { describe, expect, it } from "vitest";

import {
  buildListWindow,
  buildProjectGroupWindow,
  canExpand,
  listRows,
  sameListEntries,
  sortEntries,
} from "./list";
import { newPaging } from "./paging";
import type { ListEntry, Session, Workflow } from "./types";

function session(
  id: string,
  updatedAt: number,
  state: Session["state"] = "idle",
  createdAt = 1,
  pinned = false,
): Session {
  return {
    id,
    machine: "localpc",
    agent: "codex",
    title: id,
    state,
    pinned,
    workspace: "/w",
    worktreeDir: "",
    createdAt,
    updatedAt,
  };
}

function workflow(
  id: string,
  updatedAt: number,
  linked: Session[],
  createdAt = 1,
  pinned = false,
): Workflow {
  return {
    id,
    title: id,
    state: "idle",
    plan: "计划",
    pinned,
    createdAt,
    updatedAt,
    linkedSessions: linked,
  };
}

describe("sortEntries", () => {
  it("置顶条目在前，其余按创建时间倒序", () => {
    const entries: ListEntry[] = [
      { kind: "session", session: session("s-old", 0, "idle", 100) },
      { kind: "workflow", workflow: workflow("w", 0, [], 300) },
      { kind: "session", session: session("s-new", 0, "idle", 500) },
      { kind: "session", session: session("s-pinned", 0, "idle", 50, true) },
    ];
    expect(sortEntries(entries).map((entry) => (entry.kind === "session" ? entry.session.id : entry.workflow.id))).toEqual([
      "s-pinned",
      "s-new",
      "w",
      "s-old",
    ]);
  });
});

describe("sameListEntries", () => {
  it("内容相同返回 true，条目或字段变化返回 false", () => {
    const before: ListEntry[] = [{ kind: "session", session: session("s1", 100) }];
    const same: ListEntry[] = [{ kind: "session", session: session("s1", 100) }];
    const changed: ListEntry[] = [{ kind: "session", session: session("s1", 200) }];

    expect(sameListEntries(before, same)).toBe(true);
    expect(sameListEntries(before, changed)).toBe(false);
  });
});

describe("buildListWindow", () => {
  it("两个来源构建为统一排序的最新窗口，并更新更早标记", () => {
    const result = buildListWindow(
      newPaging(),
      [session("s1", 0, "idle", 100)],
      [workflow("w1", 0, [session("linked", 0, "idle", 150)], 200)],
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

describe("buildProjectGroupWindow", () => {
  it("两个来源按创建时间统一排序，并截断为项目组窗口", () => {
    const result = buildProjectGroupWindow(
      [session("s1", 100, "idle", 300), session("s2", 100, "idle", 100)],
      [workflow("w1", 100, [], 200)],
      2,
      false,
      false,
    );
    expect(
      result.entries.map((entry) =>
        entry.kind === "session" ? entry.session.id : entry.workflow.id,
      ),
    ).toEqual(["s1", "w1"]);
    expect(result.hasMore).toBe(true);
  });

  it("任一来源仍有更早页时保留加载更多状态", () => {
    const result = buildProjectGroupWindow(
      [session("s1", 100)],
      [],
      20,
      true,
      false,
    );
    expect(result.entries).toHaveLength(1);
    expect(result.hasMore).toBe(true);
  });

  it("置顶条目全部展示，窗口额度只限制非置顶条目", () => {
    const result = buildProjectGroupWindow(
      [
        session("pinned", 0, "idle", 100, true),
        session("new", 0, "idle", 90),
        session("old", 0, "idle", 80),
      ],
      [],
      1,
      false,
      false,
    );
    expect(
      result.entries.map((entry) =>
        entry.kind === "session" ? entry.session.id : entry.workflow.id,
      ),
    ).toEqual(["pinned", "new"]);
    expect(result.hasMore).toBe(true);
  });
});

describe("listRows", () => {
  const linked = [
    session("l-low", 0, "idle", 150),
    session("l-high", 0, "idle", 400),
  ];
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

  it("展开后关联会话置顶优先、其余按创建时间倒序，且不影响其他条目位置", () => {
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

  it("关联会话中置顶项排在最前", () => {
    const rows = listRows(
      [
        {
          kind: "workflow",
          workflow: workflow(
            "w",
            0,
            [
              session("new", 0, "idle", 300),
              session("pinned", 0, "idle", 100, true),
            ],
            1000,
          ),
        },
      ],
      new Set(["w"]),
    );
    expect(
      rows.slice(1).map((row) =>
        row.entry.kind === "session" ? row.entry.session.id : row.entry.workflow.id,
      ),
    ).toEqual(["pinned", "new"]);
  });
});
