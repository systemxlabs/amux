// 会话列表与对话的主动刷新（docs/DESIGN.md「会话列表刷新机制」）与打开会话时的面板收敛
// （docs/PRD.md「主页面」：工作目录、改动审查、计划、终端仅普通会话展示）。

import { describe, expect, it } from "vitest";

import type { ApiClient } from "../lib/api";
import type {
  FsEntry,
  ListEntry,
  Session,
  SessionList,
  Workflow,
  WorkflowList,
} from "../lib/types";
import { cancelOpen, createWorkflow, openEntry, showNewSession, updateWorkspaceInput } from "./actions";
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

it("重新打开新建视图收起面板，创建失败保留模式和完整草稿", async () => {
  const core = new Core();
  core.state.middle = "interaction";
  core.state.sidePanel = "workspace";
  core.state.newSession = {
    ...core.state.newSession,
    mode: "workflow",
    machine: "localpc",
    agent: "codex",
    workspace: "/draft",
    useWorktree: true,
    plan: "尚未创建的计划",
  };
  const draft = structuredClone(core.state.newSession);

  showNewSession(core);
  expect(core.state.middle).toBe("new");
  expect(core.state.sidePanel).toBeNull();
  expect(core.state.newSession).toEqual(draft);

  core.client = {
    createWorkflow: async () => { throw new Error("创建失败"); },
  } as unknown as ApiClient;
  await createWorkflow(core);
  expect(core.state.middle).toBe("new");
  expect(core.state.newSession).toEqual(draft);
});

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

/// 目录条目（联想列表只列目录）。
function dir(name: string, path: string): FsEntry {
  return { name, path, isDir: true, size: 0 };
}

/** 假客户端：按「机器:目录@offset」返回分页结果，并记录每次拉取的机器、目录与偏移。 */
function listingClient(
  pages: Record<string, { entries: FsEntry[]; hasMore: boolean }>,
): { client: ApiClient; calls: string[] } {
  const calls: string[] = [];
  const client = {
    listDir: async (
      machine: string,
      path: string,
      _limit?: number,
      offset = 0,
    ): Promise<{ path: string; entries: FsEntry[]; hasMore: boolean; nextOffset: number }> => {
      calls.push(`${machine}:${path}@${offset}`);
      const page = pages[`${path}@${offset}`] ?? { entries: [], hasMore: false };
      return {
        path,
        entries: page.entries,
        hasMore: page.hasMore,
        nextOffset: offset + page.entries.length,
      };
    },
  } as unknown as ApiClient;
  return { client, calls };
}

/** 工作目录输入联想（docs/DESIGN.md「新建会话视图」）。 */
describe("updateWorkspaceInput", () => {
  it("目录边界处拉取该目录全部条目（分页续拉到底）", async () => {
    const { client, calls } = listingClient({
      "/home/@0": { entries: [dir("tom", "/home/tom"), dir("tmp", "/home/tmp")], hasMore: true },
      "/home/@2": { entries: [dir("usr", "/home/usr")], hasMore: false },
    });
    const core = new Core();
    core.client = client;
    core.state.newSession.machine = "localpc";

    await updateWorkspaceInput(core, "/home/");

    expect(calls).toEqual(["localpc:/home/@0", "localpc:/home/@2"]);
    expect(core.state.newSession.suggestions.map((entry) => entry.name)).toEqual([
      "tom",
      "tmp",
      "usr",
    ]);
  });

  it("同一目录内继续输入不重复拉取，用已拉取的条目做前缀匹配", async () => {
    const { client, calls } = listingClient({
      "/@0": {
        entries: [dir("home", "/home"), dir("hola", "/hola"), dir("etc", "/etc")],
        hasMore: false,
      },
      "/home/@0": { entries: [dir("tom", "/home/tom"), dir("jerry", "/home/jerry")], hasMore: false },
    });
    const core = new Core();
    core.client = client;
    core.state.newSession.machine = "localpc";

    await updateWorkspaceInput(core, "/ho");
    await updateWorkspaceInput(core, "/hol");

    expect(calls).toEqual(["localpc:/@0"]);
    expect(core.state.newSession.suggestions.map((entry) => entry.name)).toEqual(["hola"]);

    // 跨过目录边界（末尾的 `/`）才拉下一级目录
    await updateWorkspaceInput(core, "/home/");
    await updateWorkspaceInput(core, "/home/t");

    expect(calls).toEqual(["localpc:/@0", "localpc:/home/@0"]);
    expect(core.state.newSession.suggestions.map((entry) => entry.name)).toEqual(["tom"]);
  });

  it("换机器后即使目录相同也重新拉取", async () => {
    const { client, calls } = listingClient({
      "/@0": { entries: [dir("home", "/home")], hasMore: false },
    });
    const core = new Core();
    core.client = client;
    core.state.newSession.machine = "localpc";

    await updateWorkspaceInput(core, "/ho");

    core.state.newSession.machine = "otherpc";
    await updateWorkspaceInput(core, "/ho");

    expect(calls).toEqual(["localpc:/@0", "otherpc:/@0"]);
  });
});
