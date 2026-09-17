// 会话列表与对话的主动刷新（docs/DESIGN.md「会话列表刷新机制」）与打开会话时的面板收敛
// （docs/PRD.md「主页面」：工作目录、改动审查、计划、终端仅普通会话展示）。

import { describe, expect, it } from "vitest";

import type { ApiClient } from "../lib/api";
import type { ListEntry, Session, SessionList, Workflow, WorkflowList } from "../lib/types";
import { cancelOpen, openEntry } from "./actions";
import { Core } from "./core";

function session(): Session {
  return {
    id: "s1",
    machine: "localpc",
    agent: "codex",
    title: "会话",
    state: "idle",
    workspace: "/w",
    worktreeDir: "",
    createdAt: 1,
    updatedAt: 1,
  };
}

function workflow(): Workflow {
  return {
    id: "w1",
    title: "工作流",
    state: "idle",
    plan: "计划",
    createdAt: 1,
    updatedAt: 1,
    linkedSessions: [],
  };
}

const sessionEntry: ListEntry = { kind: "session", session: session() };
const workflowEntry: ListEntry = { kind: "workflow", workflow: workflow() };

describe("openEntry", () => {
  it("切到工作流会话时关闭仅普通会话有的面板", async () => {
    const core = new Core();
    core.state.sidePanel = "workspace";

    await openEntry(core, workflowEntry);

    expect(core.state.sidePanel).toBeNull();
    expect(core.state.open).toEqual({ kind: "workflow", id: "w1" });
  });

  it("适用于工作流会话的面板在切换后保留", async () => {
    const core = new Core();
    core.state.sidePanel = "details";

    await openEntry(core, workflowEntry);

    expect(core.state.sidePanel).toBe("details");
  });

  it("切到普通会话时保留仅普通会话有的面板", async () => {
    const core = new Core();
    core.state.sidePanel = "plan";

    await openEntry(core, sessionEntry);

    expect(core.state.sidePanel).toBe("plan");
  });
});

describe("cancelOpen", () => {
  it("工作流会话取消以用户消息下发，并主动刷新对话与会话列表", async () => {
    const calls: string[] = [];
    const client = {
      promptWorkflow: async (): Promise<void> => {
        calls.push("promptWorkflow");
      },
      workflowHistory: async () => {
        calls.push("workflowHistory");
        return { items: [], hasMore: false };
      },
      sessions: async (): Promise<SessionList> => {
        calls.push("sessions");
        return { sessions: [], hasMore: false };
      },
      workflows: async (): Promise<WorkflowList> => {
        calls.push("workflows");
        return { workflows: [], hasMore: false };
      },
    } as unknown as ApiClient;
    const core = new Core();
    core.client = client;
    core.state.status = "online";
    core.state.open = { kind: "workflow", id: "w1" };

    await cancelOpen(core);

    expect(calls).toEqual(["promptWorkflow", "sessions", "workflows", "workflowHistory"]);
  });

  it("普通会话取消不发送用户消息，因此不刷新会话列表", async () => {
    const calls: string[] = [];
    const client = {
      cancelSession: async (): Promise<void> => {
        calls.push("cancelSession");
      },
      history: async () => {
        calls.push("history");
        return { items: [], hasMore: false };
      },
      sessions: async (): Promise<SessionList> => {
        calls.push("sessions");
        return { sessions: [], hasMore: false };
      },
    } as unknown as ApiClient;
    const core = new Core();
    core.client = client;
    core.state.status = "online";
    core.state.open = { kind: "session", id: "s1" };

    await cancelOpen(core);

    expect(calls).toEqual(["cancelSession", "history"]);
  });
});
