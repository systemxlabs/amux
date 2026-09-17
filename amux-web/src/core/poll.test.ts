// 会话列表刷新与翻页边界（docs/DESIGN.md「会话列表视图」）。

import { describe, expect, it, vi } from "vitest";

import type { ApiClient } from "../lib/api";
import { newPaging } from "../lib/paging";
import type { Session, SessionList, Workflow, WorkflowList } from "../lib/types";
import { Core } from "./core";
import { loadOlderList, refreshList } from "./poll";

function session(id: string, updatedAt: number): Session {
  return {
    id,
    machine: "localpc",
    agent: "codex",
    title: id,
    state: "idle",
    workspace: "/w",
    worktreeDir: "",
    createdAt: 1,
    updatedAt,
  };
}

function workflow(id: string, updatedAt: number): Workflow {
  return {
    id,
    title: id,
    state: "idle",
    plan: "计划",
    createdAt: 1,
    updatedAt,
    linkedSessions: [],
  };
}

function fakeClient(sessions: SessionList, workflows: WorkflowList): ApiClient {
  const client = {
    sessions: vi.fn(async (_limit: number, _offset: number): Promise<SessionList> => sessions),
    workflows: vi.fn(async (_limit: number, _offset: number): Promise<WorkflowList> => workflows),
  };
  return client as unknown as ApiClient;
}

describe("refreshList", () => {
  it("整窗重建，服务端已删除的旧条目被移除", async () => {
    const core = new Core();
    core.state.entries = [
      { kind: "session", session: session("gone", 300) },
      { kind: "workflow", workflow: workflow("gone-w", 200) },
    ];
    core.state.listPaging = { ...newPaging(), pageSize: 2 };
    core.client = fakeClient(
      { sessions: [session("alive", 400)], hasMore: true },
      { workflows: [], hasMore: false },
    );

    await refreshList(core);

    expect(
      core.state.entries.map((entry) =>
        entry.kind === "session" ? entry.session.id : entry.workflow.id,
      ),
    ).toEqual(["alive"]);
    expect(core.state.listPaging.hasOlder).toBe(true);
  });
});

describe("loadOlderList", () => {
  it("普通会话与工作流会话按各自已加载条数作为偏移", async () => {
    const core = new Core();
    core.state.entries = [
      { kind: "session", session: session("s1", 300) },
      { kind: "session", session: session("s2", 290) },
      { kind: "workflow", workflow: workflow("w1", 200) },
    ];
    core.state.listPaging = { ...newPaging(), pageSize: 1, hasOlder: true };
    const client = fakeClient(
      { sessions: [session("s3", 100)], hasMore: true },
      { workflows: [workflow("w2", 50)], hasMore: false },
    );
    core.client = client;

    await loadOlderList(core);

    expect(client.sessions).toHaveBeenCalledWith(1, 2);
    expect(client.workflows).toHaveBeenCalledWith(1, 1);
    expect(
      core.state.entries.map((entry) =>
        entry.kind === "session" ? entry.session.id : entry.workflow.id,
      ),
    ).toEqual(["s1", "s2", "w1", "s3", "w2"]);
    expect(core.state.listPaging.loadingOlder).toBe(false);
  });
});
