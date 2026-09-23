// 会话列表刷新与翻页边界（docs/DESIGN.md「会话列表视图」），
// 以及打开会话的刷新节拍（「会话详情视图」）与内置智能体配置读取失败的处理（docs/PRD.md「新建会话视图」）。

import { describe, expect, it, vi } from "vitest";

import type { ApiClient } from "../lib/api";
import { newPaging } from "../lib/paging";
import type {
  ContextInfo,
  HistoryPage,
  ListEntry,
  Session,
  SessionList,
  Workflow,
  WorkflowList,
} from "../lib/types";
import { Core } from "./core";
import { loadOlderList, refreshDetails, refreshList, refreshNewSession, refreshWorkflowSetup, refreshOrchestrator, refreshInteraction, stopTerminalStream, tick } from "./poll";

it("每次打开新建视图都读取最新计划，切换工作流模式不重复读取计划", async () => {
  const core = new Core();
  const workflowPlans = vi.fn().mockResolvedValueOnce([{ name: "旧计划", plan: "旧内容" }])
    .mockResolvedValueOnce([{ name: "新计划", plan: "新内容" }]);
  const machines = vi.fn(async () => [{ name: "pc" }]);
  const agents = vi.fn(async () => []);
  const recentWorkspaces = vi.fn(async () => []);
  core.client = {
    machines, agents, recentWorkspaces, workflowPlans,
    orchestrator: vi.fn(async () => null),
  } as unknown as ApiClient;

  await refreshNewSession(core);
  await refreshWorkflowSetup(core);
  expect(workflowPlans).toHaveBeenCalledTimes(1);
  await refreshNewSession(core);
  expect(workflowPlans).toHaveBeenCalledTimes(2);
  expect(core.state.settings.plans).toEqual([{ name: "新计划", plan: "新内容" }]);
  expect(machines).toHaveBeenCalledTimes(2);
  expect(agents).toHaveBeenCalledWith("pc");
  expect(recentWorkspaces).toHaveBeenCalledTimes(2);
});

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
    expect(core.state.listRefreshVersion).toBe(1);
  });

  it("刷新内容未变化时保留原列表引用", async () => {
    const original: ListEntry = { kind: "session", session: session("s1", 100) };
    const core = new Core();
    core.state.entries = [original];
    core.client = fakeClient(
      { sessions: [session("s1", 100)], hasMore: false },
      { workflows: [], hasMore: false },
    );

    await refreshList(core);

    expect(core.state.entries[0]).toBe(original);
    expect(core.state.listRefreshVersion).toBe(1);
  });

  it("普通请求失败时保留登录状态并提示错误", async () => {
    const core = new Core();
    const client = {
      sessions: vi.fn(async () => {
        throw new Error("network down");
      }),
      workflows: vi.fn(async () => ({ workflows: [], hasMore: false })),
    } as unknown as ApiClient;
    core.client = client;
    core.state.status = "online";

    await refreshList(core);

    expect(core.client).toBe(client);
    expect(core.state.status).toBe("online");
    expect(core.state.notice).toEqual({
      kind: "error",
      text: "刷新会话列表失败：network down",
    });
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

// ---------- 打开会话的刷新节拍与内置智能体配置 ----------

/** 记录被调用方法名的假客户端；处理器返回 Error 表示该方法拒绝。 */
function recordingClient(overrides: Record<string, unknown> = {}): {
  client: ApiClient;
  calls: string[];
} {
  const calls: string[] = [];
  const handlers: Record<string, unknown> = {
    sessions: (): SessionList => ({ sessions: [session("s1", 1)], hasMore: false }),
    workflows: (): WorkflowList => ({ workflows: [], hasMore: false }),
    history: (): HistoryPage => ({ items: [], hasMore: false }),
    session: () => session("s1", 1),
    workflow: () => workflow("w1", 1),
    context: (): ContextInfo => ({ contextSize: 10, contextWindowSize: 100 }),
    ...overrides,
  };
  const client: Record<string, unknown> = {};
  for (const [name, handler] of Object.entries(handlers)) {
    client[name] = async (...args: unknown[]): Promise<unknown> => {
      calls.push(name);
      const result =
        typeof handler === "function" ? (handler as (...a: unknown[]) => unknown)(...args) : handler;
      if (result instanceof Error) throw result;
      return result;
    };
  }
  return { client: client as unknown as ApiClient, calls };
}

function onlineCore(client: ApiClient): Core {
  const core = new Core();
  core.client = client;
  core.state.status = "online";
  return core;
}

describe("tick", () => {
  it("打开时读取控件数据，只有实时活动每两秒刷新，重开才重新读取控件", async () => {
    vi.useFakeTimers();
    try {
      const { client, calls } = recordingClient({
        machines: () => [], orchestrator: () => null, quickCommands: () => [],
        configOptions: () => [], slashCommands: () => [], ongoingActivity: () => null,
      });
      const core = onlineCore(client);
      core.state.middle = "interaction";
      core.state.open = { kind: "session", id: "s1" };
      await Promise.all([refreshInteraction(core), tick(core)]);
      const count = (name: string) => calls.filter((call) => call === name).length;
      expect(count("ongoingActivity")).toBe(1);
      vi.advanceTimersByTime(1999);
      await tick(core);
      expect(count("ongoingActivity")).toBe(1);
      vi.advanceTimersByTime(1);
      await tick(core);
      expect(count("ongoingActivity")).toBe(2);
      vi.advanceTimersByTime(10_000);
      await tick(core);
      for (const name of ["quickCommands", "configOptions", "slashCommands"]) {
        expect(count(name)).toBe(1);
      }
      await refreshInteraction(core);
      for (const name of ["quickCommands", "configOptions", "slashCommands"]) {
        expect(count(name)).toBe(2);
      }
    } finally {
      vi.useRealTimers();
    }
  });
  it("详情视图未打开时，只刷新会话列表与对话历史，不拉取会话详情与上下文", async () => {
    const { client, calls } = recordingClient();
    const core = onlineCore(client);
    core.state.middle = "interaction";
    core.state.open = { kind: "session", id: "s1" };
    core.state.sidePanel = null;

    await tick(core);

    expect(calls).toContain("history");
    expect(calls).not.toContain("session");
    expect(calls).not.toContain("context");
  });

  it("详情视图打开时也不定时刷新详情，由视图打开时自行拉取一次", async () => {
    const { client, calls } = recordingClient();
    const core = onlineCore(client);
    core.state.middle = "interaction";
    core.state.open = { kind: "session", id: "s1" };
    core.state.sidePanel = "details";

    await tick(core);

    expect(calls).not.toContain("session");
    expect(calls).not.toContain("context");
  });
});

describe("tick 终端流", () => {
  it("终端视图打开期间建立 SSE，不周期性拉取终端列表或输出", async () => {
    let streamSignal: AbortSignal | undefined;
    const { client, calls } = recordingClient({
      ongoingActivity: () => null,
      terminalOutputStream: (
        _id: string,
        _terminal: string,
        signal: AbortSignal,
        onOutput: (output: { data: string; nextCursor: number }) => void,
      ) =>
        new Promise<void>((resolve) => {
          streamSignal = signal;
          onOutput({ data: "aGk=", nextCursor: 2 });
          onOutput({ data: "IQ==", nextCursor: 3 });
          signal.addEventListener("abort", () => resolve(), { once: true });
        }),
    });
    const core = onlineCore(client);
    core.state.middle = "interaction";
    core.state.open = { kind: "session", id: "s1" };
    core.state.sidePanel = "terminal";
    core.state.detail.activeTerminal = "t1";

    await tick(core);
    expect(calls).toContain("terminalOutputStream");
    expect(calls).not.toContain("terminals");
    expect(core.state.detail.terminalChunks).toHaveLength(2);
    expect(new TextDecoder().decode(core.state.detail.terminalChunks[0].bytes)).toBe("hi");
    expect(core.state.detail.terminalChunks[0].reset).toBe(true);
    expect(new TextDecoder().decode(core.state.detail.terminalChunks[1].bytes)).toBe("!");
    expect(core.state.detail.terminalChunks[1].reset).toBe(false);

    await tick(core);
    expect(calls.filter((call) => call === "terminalOutputStream")).toHaveLength(1);
    expect(streamSignal?.aborted).toBe(false);
    stopTerminalStream(core);
    expect(streamSignal?.aborted).toBe(true);
  });
});

describe("refreshDetails", () => {
  it("普通会话：拉取一次详情与上下文用量", async () => {
    const { client, calls } = recordingClient();
    const core = onlineCore(client);
    core.state.open = { kind: "session", id: "s1" };

    await refreshDetails(core);

    expect(calls).toEqual(["session", "context"]);
    expect(core.state.detail.session?.id).toBe("s1");
    expect(core.state.detail.contextSize).toBe(10);
    expect(core.state.detail.contextWindowSize).toBe(100);
  });

  it("工作流会话：只拉取详情，不请求上下文", async () => {
    const { client, calls } = recordingClient();
    const core = onlineCore(client);
    core.state.open = { kind: "workflow", id: "w1" };

    await refreshDetails(core);

    expect(calls).toEqual(["workflow"]);
    expect(core.state.detail.workflow?.id).toBe("w1");
  });
});

describe("refreshOrchestrator", () => {
  it("读取成功：记录已确认配置（null 表示未配置）", async () => {
    const { client } = recordingClient({ orchestrator: () => null });
    const core = onlineCore(client);

    await refreshOrchestrator(core);

    expect(core.state.settings.orchestrator).toEqual({ status: "ready", config: null });
  });

  it("读取失败：记录失败状态而非停留在未确认，视图据此拒绝创建工作流会话", async () => {
    const { client } = recordingClient({ orchestrator: () => new Error("连接超时") });
    const core = onlineCore(client);

    await refreshOrchestrator(core);

    expect(core.state.settings.orchestrator).toEqual({ status: "failed", error: "连接超时" });
  });
});
