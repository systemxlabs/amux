// 会话列表与对话的主动刷新（docs/DESIGN.md「会话列表刷新机制」）。

import { describe, expect, it } from "vitest";

import type { ApiClient } from "../lib/api";
import type { SessionList, WorkflowList } from "../lib/types";
import { cancelOpen } from "./actions";
import { Core } from "./core";

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
