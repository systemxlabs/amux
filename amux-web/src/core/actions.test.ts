// 会话列表与对话的主动刷新（docs/DESIGN.md「会话列表刷新机制」）与打开会话时的面板收敛
// （docs/PRD.md「主页面」：工作目录、改动审查、计划、终端仅普通会话展示）。

import { describe, expect, it } from "vitest";

import type { ApiClient } from "../lib/api";
import type {
  ContentBlock,
  FsEntry,
  ListEntry,
  Session,
  SessionList,
  Workflow,
  WorkflowList,
} from "../lib/types";
import {
  addFiles,
  cancelOpen,
  createWorkflow,
  deleteRecentWorkspace,
  deleteProject,
  openEntry,
  openTerminal,
  retryAttachment,
  sendPrompt,
  showNewSession,
  toggleSidePanel,
  updateWorkspaceInput,
} from "./actions";
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

it("附件上传成功后以待发送 ResourceLink 保存，不再内联文件内容", async () => {
  const core = new Core();
  core.state.open = { kind: "session", id: "s1" };
  core.client = {
    uploadSessionAttachment: async () => ({
      name: "uuid.txt",
      size: 5,
      createdAt: 1,
    }),
    sessionAttachmentUri: () =>
      "https://amux.example.com/sessions/s1/attachments/uuid.txt",
  } as unknown as ApiClient;

  await addFiles(core, [new File(["hello"], "note.txt", { type: "text/plain" })]);

  expect(core.state.attachments).toHaveLength(1);
  expect(core.state.attachments[0]).toMatchObject({
      id: expect.any(String),
      file: expect.any(File),
      status: "uploaded",
      block: {
        type: "resource_link",
        uri: "https://amux.example.com/sessions/s1/attachments/uuid.txt",
        name: "note.txt",
        mimeType: "text/plain",
      },
      label: "note.txt",
      remoteName: "uuid.txt",
  });
});

it("附件上传失败后保留文件并可重试", async () => {
  const core = new Core();
  core.state.open = { kind: "session", id: "s1" };
  let attempts = 0;
  core.client = {
    uploadSessionAttachment: async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("network down");
      return { name: "uuid.txt", size: 5, createdAt: 1 };
    },
    sessionAttachmentUri: () =>
      "https://amux.example.com/sessions/s1/attachments/uuid.txt",
  } as unknown as ApiClient;

  await addFiles(core, [new File(["hello"], "note.txt", { type: "text/plain" })]);
  expect(core.state.attachments[0]?.status).toBe("failed");
  expect(core.state.attachments[0]?.error).toContain("network down");

  await retryAttachment(core, core.state.attachments[0]!.id);
  expect(core.state.attachments[0]).toMatchObject({
    status: "uploaded",
    remoteName: "uuid.txt",
  });
});

it("删除项目时同步移除其快捷指令", async () => {
  const core = new Core();
  core.state.settings.projects = [
    { name: "project-a", description: "" },
    { name: "project-b", description: "" },
  ];
  core.state.settings.quickCommands = [
    { project: "project-a", name: "项目指令", prompt: "a" },
    { project: "project-b", name: "项目指令", prompt: "b" },
    { name: "通用指令", prompt: "general" },
  ];
  core.client = {
    deleteProject: async (): Promise<void> => {},
    sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
    workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
  } as unknown as ApiClient;

  await deleteProject(core, "project-a");

  expect(core.state.settings.projects.map((project) => project.name)).toEqual(["project-b"]);
  expect(core.state.settings.quickCommands).toEqual([
    { project: "project-b", name: "项目指令", prompt: "b" },
    { name: "通用指令", prompt: "general" },
  ]);
});

it("删除最近工作目录：仅保留剩余项并全量保存", async () => {
  const core = new Core();
  core.state.recentWorkspaces = [
    { machine: "localpc", workspace: "/w1", lastUsed: 1 },
    { machine: "localpc", workspace: "/w2", lastUsed: 2 },
  ];
  const sent: unknown[] = [];
  core.client = {
    setRecentWorkspaces: async (list: unknown[]): Promise<void> => {
      sent.push(list);
    },
  } as unknown as ApiClient;

  await deleteRecentWorkspace(core, "localpc", "/w1");

  expect(sent).toEqual([
    [{ machine: "localpc", workspace: "/w2", lastUsed: 2 }],
  ]);
  expect(core.state.recentWorkspaces).toEqual([
    { machine: "localpc", workspace: "/w2", lastUsed: 2 },
  ]);
});

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

describe("sendPrompt", () => {
  /** 记录 prompt 载荷并返回最小可用的假客户端。 */
  function promptingClient(): { client: ApiClient; prompts: ContentBlock[][] } {
    const prompts: ContentBlock[][] = [];
    const client = {
      promptSession: async (_id: string, input: ContentBlock[]) => {
        prompts.push(input);
      },
      history: async () => ({ items: [], hasMore: false }),
      sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
      workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
    } as unknown as ApiClient;
    return { client, prompts };
  }

  function withAttachments(core: Core): void {
    core.update((state) => {
      state.attachments = [
        {
          id: "pending-1",
          file: new File(["draft"], "draft.txt", { type: "text/plain" }),
          block: { type: "text", text: "draft.txt" },
          label: "draft.txt",
          status: "uploaded",
          remoteName: "remote.txt",
        },
      ];
    });
  }

  it("快捷指令只发送预设提示词，不携带也不清空待发送附件", async () => {
    const { client, prompts } = promptingClient();
    const core = new Core();
    core.client = client;
    core.state.open = { kind: "session", id: "s1" };
    withAttachments(core);

    await sendPrompt(core, "检查构建", false);

    expect(prompts).toEqual([[{ type: "text", text: "检查构建" }]]);
    // 未打算提交的附件保留在输入区，供后续手动发送。
    expect(core.state.attachments.map((attachment) => attachment.label)).toEqual(["draft.txt"]);
  });

  it("输入框发送仍一并携带附件并在成功后清空", async () => {
    const { client, prompts } = promptingClient();
    const core = new Core();
    core.client = client;
    core.state.open = { kind: "session", id: "s1" };
    withAttachments(core);

    const ok = await sendPrompt(core, "看一下这个文件");

    expect(ok).toBe(true);
    expect(prompts).toEqual([
      [
        { type: "text", text: "看一下这个文件" },
        { type: "text", text: "draft.txt" },
      ],
    ]);
    expect(core.state.attachments).toEqual([]);
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

describe("终端视图", () => {
  it("打开终端面板与打开终端时追加 chunk 都更换数组引用", async () => {
    const core = new Core();
    const before = core.state.detail.terminalChunks;
    toggleSidePanel(core, "terminal");
    const after = core.state.detail.terminalChunks;
    expect(after).not.toBe(before);
    expect(after).toHaveLength(1);

    core.client = {
      openTerminal: async () => "t1",
      terminals: async () => [
        { id: "t1", cwd: "/w", cols: 80, rows: 24, state: "running" },
      ],
    } as unknown as ApiClient;
    core.state.open = { kind: "session", id: "s1" };
    core.state.detail.session = { ...session(), id: "s1" };
    const prior = core.state.detail.terminalChunks;
    await openTerminal(core, 80, 24);
    expect(core.state.detail.terminalChunks).not.toBe(prior);
    expect(core.state.detail.terminalChunks).toHaveLength(2);
    expect(core.state.detail.activeTerminal).toBe("t1");
  });
});
